import AVFoundation
import CoreMedia
import UIKit

/// Hardware H.264 decode and display: each AVCC sample from Rust becomes a `CMSampleBuffer`
/// for an `AVSampleBufferDisplayLayer`, which decodes and shows it without the pixels ever
/// passing through the app.
final class VideoRenderer: @unchecked Sendable {
    let layer = AVSampleBufferDisplayLayer()
    var onNeedKeyframe: (() -> Void)?
    var onVideoSize: ((CGSize) -> Void)?

    private let queue = DispatchQueue(label: "app.rconn.video", qos: .userInteractive)
    private var format: CMVideoFormatDescription?
    private var sps: Data?
    private var pps: Data?
    private var size = CGSize.zero
    private let lock = NSLock()
    private var enqueued = 0

    var framesEnqueued: Int {
        lock.lock()
        defer { lock.unlock() }
        return enqueued
    }

    init() {
        layer.videoGravity = .resizeAspect
        layer.backgroundColor = UIColor.black.cgColor
    }

    func enqueue(_ frame: VideoFrame) {
        queue.async { self.render(frame) }
    }

    /// In the background: drop pictures (nothing shows them; saves the decoder's work).
    private var paused = false
    /// Back from a pause: nothing to decode against until a keyframe.
    private var needKeyframe = false

    func pause() {
        queue.async { self.paused = true }
    }

    func resume() {
        queue.async {
            self.paused = false
            self.needKeyframe = true
        }
    }

    private func render(_ f: VideoFrame) {
        if paused { return }
        if needKeyframe {
            guard f.keyframe else {
                onNeedKeyframe?()
                return
            }
            needKeyframe = false
        }
        if let s = f.sps, let p = f.pps, s != sps || p != pps {
            format = Self.makeFormat(sps: s, pps: p)
            (sps, pps) = (s, p)
            if let format {
                let d = CMVideoFormatDescriptionGetDimensions(format)
                let newSize = CGSize(width: Int(d.width), height: Int(d.height))
                if newSize != size {
                    size = newSize
                    onVideoSize?(newSize)
                }
            }
        }
        guard let format else {
            onNeedKeyframe?()
            return
        }
        let r = layer.sampleBufferRenderer
        if r.status == .failed || r.requiresFlushToResumeDecoding {
            // E.g. after the app was in the background: start again from a keyframe.
            NSLog("rconn: video renderer needs a flush (%@)", r.error?.localizedDescription ?? "-")
            r.flush()
            if !f.keyframe {
                onNeedKeyframe?()
                return
            }
        }
        guard let sample = Self.makeSample(f, format: format) else { return }
        r.enqueue(sample)
        lock.lock()
        enqueued += 1
        let n = enqueued
        lock.unlock()
        if n == 1 || n % 100 == 0 {
            logProgress(n)
        }
    }

    /// Log the frame count and what the layer actually shows (proves it decodes).
    private func logProgress(_ n: Int) {
        let r = layer.sampleBufferRenderer
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.1) {
            guard #available(iOS 17.4, *) else {
                NSLog("rconn: %d frames enqueued (status %d)", n, r.status.rawValue)
                return
            }
            if let px = r.displayedPixelBuffer() {
                NSLog("rconn: %d frames enqueued; showing %dx%d", n,
                      CVPixelBufferGetWidth(px), CVPixelBufferGetHeight(px))
            } else {
                NSLog("rconn: %d frames enqueued; nothing displayed yet (status %d)", n, r.status.rawValue)
            }
        }
    }

    private static func makeFormat(sps: Data, pps: Data) -> CMVideoFormatDescription? {
        var fmt: CMVideoFormatDescription?
        let status = sps.withUnsafeBytes { (s: UnsafeRawBufferPointer) in
            pps.withUnsafeBytes { (p: UnsafeRawBufferPointer) -> OSStatus in
                let ptrs = [s.bindMemory(to: UInt8.self).baseAddress!, p.bindMemory(to: UInt8.self).baseAddress!]
                let sizes = [sps.count, pps.count]
                return CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    allocator: kCFAllocatorDefault, parameterSetCount: 2,
                    parameterSetPointers: ptrs, parameterSetSizes: sizes,
                    nalUnitHeaderLength: 4, formatDescriptionOut: &fmt)
            }
        }
        if status != noErr {
            NSLog("rconn: bad SPS/PPS (%d)", status)
            return nil
        }
        return fmt
    }

    private static func makeSample(_ f: VideoFrame, format: CMVideoFormatDescription) -> CMSampleBuffer? {
        let len = f.avcc.count
        guard len > 0 else { return nil }
        var block: CMBlockBuffer?
        guard CMBlockBufferCreateWithMemoryBlock(
            allocator: kCFAllocatorDefault, memoryBlock: nil, blockLength: len,
            blockAllocator: kCFAllocatorDefault, customBlockSource: nil, offsetToData: 0,
            dataLength: len, flags: kCMBlockBufferAssureMemoryNowFlag, blockBufferOut: &block) == noErr,
            let block
        else { return nil }
        let copied = f.avcc.withUnsafeBytes { raw in
            CMBlockBufferReplaceDataBytes(
                with: raw.baseAddress!, blockBuffer: block, offsetIntoDestination: 0, dataLength: len)
        }
        guard copied == noErr else { return nil }
        var timing = CMSampleTimingInfo(
            duration: .invalid,
            presentationTimeStamp: CMTime(value: CMTimeValue(f.ptsUs), timescale: 1_000_000),
            decodeTimeStamp: .invalid)
        var sampleSize = len
        var sample: CMSampleBuffer?
        guard CMSampleBufferCreateReady(
            allocator: kCFAllocatorDefault, dataBuffer: block, formatDescription: format,
            sampleCount: 1, sampleTimingEntryCount: 1, sampleTimingArray: &timing,
            sampleSizeEntryCount: 1, sampleSizeArray: &sampleSize, sampleBufferOut: &sample) == noErr,
            let sample
        else { return nil }
        // Show each picture as soon as it is decoded: this is live video, not a movie.
        if let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: true),
           CFArrayGetCount(attachments) > 0
        {
            let dict = unsafeBitCast(CFArrayGetValueAtIndex(attachments, 0), to: CFMutableDictionary.self)
            CFDictionarySetValue(
                dict, Unmanaged.passUnretained(kCMSampleAttachmentKey_DisplayImmediately).toOpaque(),
                Unmanaged.passUnretained(kCFBooleanTrue).toOpaque())
            if !f.keyframe {
                CFDictionarySetValue(
                    dict, Unmanaged.passUnretained(kCMSampleAttachmentKey_NotSync).toOpaque(),
                    Unmanaged.passUnretained(kCFBooleanTrue).toOpaque())
            }
        }
        return sample
    }
}
