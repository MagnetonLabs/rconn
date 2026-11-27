import SwiftUI
import UIKit
import UniformTypeIdentifiers

/// One download or upload, as the Files sheet shows it.
struct TransferItem: Identifiable {
    enum Outcome {
        case saved(String)
        case failed(String)
        case cancelled
    }

    let id = UUID()
    let upload: Bool
    let name: String
    let transfer: Transfer
    var outcome: Outcome?
    /// A finished download, here on the device.
    var localURL: URL?
}

/// A file to hand to the share sheet.
struct SharedFile: Identifiable {
    let url: URL
    var id: URL { url }
}

/// The computer's folders and the transfers, for the Files sheet. Downloads go to the
/// app's Documents (shown in the Files app); uploads into the folder shown. Rust does the
/// transfer and checks each file's SHA-256 (`Session.download` / `upload`).
final class FilesModel: ObservableObject {
    private weak var session: Session?
    /// The remote folder shown ("" is the computer's shared folder, usually home).
    @Published private(set) var folder = remoteStartFolder()
    @Published private(set) var entries: [RemoteEntry]?
    @Published private(set) var error: String?
    @Published private(set) var transfers: [TransferItem] = []
    @Published var share: SharedFile?
    private var opened = false

    init(session: Session?) {
        self.session = session
    }

    var title: String { remoteFolderTitle(folder: folder) }
    var atRoot: Bool { folder.isEmpty }

    /// The first time the sheet shows: the remote Downloads, or the root if it has none.
    func open() {
        guard !opened else { return }
        opened = true
        show(folder, fallbackToRoot: true)
    }

    func refresh() { show(folder) }
    func up() { show(remoteParent(folder: folder)) }

    func tap(_ e: RemoteEntry) {
        guard let path = remoteChild(folder: folder, name: e.name) else { return }
        if e.isDir { show(path) } else { download(path, name: e.name) }
    }

    private func show(_ dir: String, fallbackToRoot: Bool = false) {
        guard let session else { return }
        folder = dir
        entries = nil
        error = nil
        Task { @MainActor in
            do {
                let list = try await session.listFolder(folder: dir)
                guard self.folder == dir else { return }
                self.entries = list
            } catch {
                guard self.folder == dir else { return }
                if fallbackToRoot && !dir.isEmpty {
                    self.show("")
                } else {
                    self.entries = []
                    self.error = Self.describe(error)
                }
            }
        }
    }

    private func download(_ remote: String, name: String) {
        guard let session else { return }
        let docs = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
        let item = TransferItem(upload: false, name: name, transfer: Transfer())
        transfers.insert(item, at: 0)
        Task { @MainActor in
            do {
                let saved = try await session.download(
                    remote: remote, destDir: docs.path(percentEncoded: false), transfer: item.transfer)
                let url = URL(fileURLWithPath: saved)
                self.finish(item.id, .saved(url.lastPathComponent), local: url)
                self.share = SharedFile(url: url)
            } catch {
                self.finish(item.id, Self.outcome(error))
            }
        }
    }

    /// Send a file the document picker gave (security-scoped) into the folder shown.
    func upload(_ url: URL) {
        guard let session else { return }
        let dir = folder
        let scoped = url.startAccessingSecurityScopedResource()
        let item = TransferItem(upload: true, name: url.lastPathComponent, transfer: Transfer())
        transfers.insert(item, at: 0)
        Task { @MainActor in
            defer { if scoped { url.stopAccessingSecurityScopedResource() } }
            do {
                let at = try await session.upload(
                    localPath: url.path(percentEncoded: false), folder: dir, transfer: item.transfer)
                self.finish(item.id, .saved(at))
                if self.folder == dir { self.refresh() }
            } catch {
                self.finish(item.id, Self.outcome(error))
            }
        }
    }

    func cancel(_ item: TransferItem) {
        item.transfer.cancel()
    }

    func clearFinished() {
        transfers.removeAll { $0.outcome != nil }
    }

    private func finish(_ id: UUID, _ outcome: TransferItem.Outcome, local: URL? = nil) {
        guard let i = transfers.firstIndex(where: { $0.id == id }) else { return }
        transfers[i].outcome = outcome
        transfers[i].localURL = local
    }

    private static func outcome(_ error: Error) -> TransferItem.Outcome {
        if case FileError.Cancelled = error { return .cancelled }
        return .failed(describe(error))
    }

    static func describe(_ error: Error) -> String {
        switch error {
        case FileError.Cancelled: return "cancelled"
        case let FileError.Failed(message): return message
        default: return error.localizedDescription
        }
    }
}

/// The Files sheet: the computer's folder (tap a folder to open it, a file to download it),
/// an Upload button, and the transfers with their progress.
struct FilesView: View {
    @ObservedObject var files: FilesModel
    @Environment(\.dismiss) private var dismiss
    @State private var picking = false

    var body: some View {
        NavigationStack {
            List {
                if !files.transfers.isEmpty {
                    Section {
                        ForEach(files.transfers) { t in
                            TransferRow(item: t, files: files)
                        }
                    } header: {
                        HStack {
                            Text("Transfers")
                            Spacer()
                            if files.transfers.contains(where: { $0.outcome != nil }) {
                                Button("Clear") { files.clearFinished() }
                                    .font(.caption)
                            }
                        }
                    }
                }
                Section {
                    if let entries = files.entries {
                        if let error = files.error {
                            Label(error, systemImage: "exclamationmark.triangle")
                                .foregroundStyle(.secondary)
                        } else if entries.isEmpty {
                            Text("This folder is empty").foregroundStyle(.secondary)
                        }
                        ForEach(entries, id: \.name) { e in
                            Button { files.tap(e) } label: { EntryRow(entry: e) }
                                .tint(.primary)
                                .accessibilityIdentifier("entry-\(e.name)")
                        }
                    } else {
                        HStack {
                            ProgressView()
                            Text("Loading…").foregroundStyle(.secondary)
                        }
                    }
                } header: {
                    Text("On the computer")
                } footer: {
                    Text("Tap a file to download it to Rconn's folder in the Files app. Uploads go into this folder.")
                }
            }
            .navigationTitle(files.title)
            .navigationBarTitleDisplayMode(.inline)
            .refreshable { files.refresh() }
            .toolbar {
                ToolbarItem(placement: .topBarLeading) {
                    Button { files.up() } label: { Label("Up", systemImage: "chevron.up") }
                        .disabled(files.atRoot)
                        .accessibilityIdentifier("filesUp")
                }
                ToolbarItem(placement: .principal) {
                    Text(files.title).font(.headline.monospaced()).lineLimit(1).truncationMode(.head)
                }
                ToolbarItemGroup(placement: .topBarTrailing) {
                    Button { picking = true } label: { Label("Upload", systemImage: "square.and.arrow.up") }
                        .accessibilityIdentifier("upload")
                    Button("Done") { dismiss() }
                        .accessibilityIdentifier("filesDone")
                }
            }
            .fileImporter(isPresented: $picking, allowedContentTypes: [.item], allowsMultipleSelection: true) { result in
                if case let .success(urls) = result {
                    urls.forEach(files.upload)
                }
            }
            .sheet(item: $files.share) { f in
                ShareSheet(url: f.url).ignoresSafeArea()
            }
        }
        .onAppear { files.open() }
    }
}

private struct EntryRow: View {
    let entry: RemoteEntry

    var body: some View {
        HStack {
            Image(systemName: entry.isDir ? "folder.fill" : "doc")
                .foregroundStyle(entry.isDir ? Color.accentColor : .secondary)
                .frame(width: 24)
            Text(entry.name).lineLimit(1).truncationMode(.middle)
            Spacer()
            if entry.isDir {
                Image(systemName: "chevron.right").font(.caption).foregroundStyle(.tertiary)
            } else {
                Text(ByteCountFormatter.string(fromByteCount: Int64(entry.size), countStyle: .file))
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(.secondary)
            }
        }
    }
}

private struct TransferRow: View {
    let item: TransferItem
    let files: FilesModel

    var body: some View {
        HStack {
            Image(systemName: item.upload ? "arrow.up.circle" : "arrow.down.circle")
                .foregroundStyle(.secondary)
            VStack(alignment: .leading, spacing: 4) {
                Text(item.name).lineLimit(1).truncationMode(.middle)
                switch item.outcome {
                case nil:
                    // Rust updates the counts; redraw a few times a second.
                    TimelineView(.periodic(from: .now, by: 0.25)) { _ in
                        let done = item.transfer.doneBytes(), total = item.transfer.totalBytes()
                        VStack(alignment: .leading, spacing: 2) {
                            if total > 0 {
                                ProgressView(value: Double(done), total: Double(total))
                            } else {
                                ProgressView().progressViewStyle(.linear)
                            }
                            Text(progressText(done, total))
                                .font(.caption.monospacedDigit()).foregroundStyle(.secondary)
                        }
                    }
                case let .saved(place)?:
                    Text(item.upload ? "Saved on the computer: \(place)" : "Downloaded, checked")
                        .font(.caption).foregroundStyle(.green)
                case let .failed(why)?:
                    Text(why).font(.caption).foregroundStyle(.red)
                case .cancelled?:
                    Text("Cancelled").font(.caption).foregroundStyle(.secondary)
                }
            }
            Spacer()
            if item.outcome == nil {
                Button { files.cancel(item) } label: { Image(systemName: "xmark.circle.fill") }
                    .buttonStyle(.borderless)
                    .tint(.secondary)
                    .accessibilityLabel("Cancel")
            } else if let url = item.localURL {
                Button { files.share = SharedFile(url: url) } label: { Image(systemName: "square.and.arrow.up") }
                    .buttonStyle(.borderless)
                    .accessibilityLabel("Share")
            }
        }
    }

    private func progressText(_ done: UInt64, _ total: UInt64) -> String {
        let d = ByteCountFormatter.string(fromByteCount: Int64(done), countStyle: .file)
        guard total > 0 else { return d }
        let t = ByteCountFormatter.string(fromByteCount: Int64(total), countStyle: .file)
        return "\(d) of \(t)"
    }
}

/// The system share sheet for a downloaded file (Save to Files, AirDrop, open in…).
struct ShareSheet: UIViewControllerRepresentable {
    let url: URL

    func makeUIViewController(context: Context) -> UIActivityViewController {
        UIActivityViewController(activityItems: [url], applicationActivities: nil)
    }

    func updateUIViewController(_ vc: UIActivityViewController, context: Context) {}
}
