//! openh264 options the `openh264` crate does not wrap: live rate changes, long-term
//! reference (LTR) feedback, several slices, fixed-quantizer frames. Each is a `SetOption`
//! call on an
//! initialized encoder.
//!
//! Unsafe is confined to this module: `SetOption` with a pointer to a correctly typed,
//! live local, on an encoder that has encoded at least one frame (initialized).
#![allow(unsafe_code)]

use openh264::encoder::Encoder;
use openh264_sys2::{
    ENCODER_LTR_MARKING_FEEDBACK, ENCODER_LTR_RECOVERY_REQUEST, ENCODER_OPTION_BITRATE,
    ENCODER_OPTION_MAX_BITRATE, ENCODER_OPTION_RC_MODE, ENCODER_OPTION_SVC_ENCODE_PARAM_EXT,
    LTR_MARKING_SUCCESS, LTR_RECOVERY_REQUEST, RC_BITRATE_MODE, RC_MODES, RC_OFF_MODE,
    SBitrateInfo, SEncParamExt, SLTRMarkingFeedback, SLTRRecoverRequest, SM_FIXEDSLCNUM_SLICE,
    SPATIAL_LAYER_0, SPATIAL_LAYER_ALL,
};
use std::os::raw::c_int;

fn set_rate(
    enc: &mut Encoder,
    option: openh264_sys2::ENCODER_OPTION,
    layer: c_int,
    bps: u32,
) -> bool {
    let mut info = SBitrateInfo {
        iLayer: layer,
        iBitrate: bps.min(i32::MAX as u32) as c_int,
    };
    // SAFETY: `info` is the type this option takes and outlives the call.
    unsafe { enc.raw_api().set_option(option, (&raw mut info).cast()) == 0 }
}

/// Change the target bitrate without restarting the encoder (no keyframe). The layer's
/// maximum moves with it: openh264 rejects a target above the maximum, so the maximum is
/// raised first when going up and lowered last when going down.
pub fn set_bitrate(enc: &mut Encoder, old_bps: u32, bps: u32) -> bool {
    // A little headroom above the target: openh264 undershoots when the two are equal.
    let max = bps.saturating_add(bps / 4);
    if bps >= old_bps {
        set_rate(enc, ENCODER_OPTION_MAX_BITRATE, SPATIAL_LAYER_0, max)
            & set_rate(enc, ENCODER_OPTION_MAX_BITRATE, SPATIAL_LAYER_ALL, max)
            & set_rate(enc, ENCODER_OPTION_BITRATE, SPATIAL_LAYER_ALL, bps)
    } else {
        set_rate(enc, ENCODER_OPTION_BITRATE, SPATIAL_LAYER_ALL, bps)
            & set_rate(enc, ENCODER_OPTION_MAX_BITRATE, SPATIAL_LAYER_0, max)
            & set_rate(enc, ENCODER_OPTION_MAX_BITRATE, SPATIAL_LAYER_ALL, max)
    }
}

/// The decoder has the long-term reference picture `frame_num` (of IDR period
/// `idr_pic_id`): the encoder may rely on it for recovery and mark the next one.
pub fn ltr_received(enc: &mut Encoder, idr_pic_id: u32, frame_num: u32) {
    let mut fb = SLTRMarkingFeedback {
        uiFeedbackType: LTR_MARKING_SUCCESS as u32,
        uiIDRPicId: idr_pic_id,
        iLTRFrameNum: frame_num as c_int,
        iLayerId: 0,
    };
    // SAFETY: as in `set_rate`.
    unsafe {
        enc.raw_api()
            .set_option(ENCODER_LTR_MARKING_FEEDBACK, (&raw mut fb).cast());
    }
}

/// Pictures after `last_correct` (up to `current`) were lost: the next picture predicts
/// from a confirmed long-term reference instead, or is an IDR if there is none.
pub fn ltr_recover(enc: &mut Encoder, idr_pic_id: u32, last_correct: u32, current: u32) {
    let mut req = SLTRRecoverRequest {
        uiFeedbackType: LTR_RECOVERY_REQUEST as u32,
        uiIDRPicId: idr_pic_id,
        iLastCorrectFrameNum: last_correct as c_int,
        iCurrentFrameNum: current as c_int,
        iLayerId: 0,
    };
    // SAFETY: as in `set_rate`.
    unsafe {
        enc.raw_api()
            .set_option(ENCODER_LTR_RECOVERY_REQUEST, (&raw mut req).cast());
    }
}

/// Cut every picture into `n` slices (horizontal bands) encoded on up to `n` threads. The
/// `openh264` crate only offers one slice, which keeps openh264 on one core whatever its
/// thread setting. Re-initializes the encoder: the next picture is an IDR.
pub fn set_slices(enc: &mut Encoder, n: u32) -> bool {
    // SAFETY: `SEncParamExt` is plain data (numbers, flags, fixed arrays), so all zeroes
    // is a valid value; GetOption fills it from the encoder's own parameters (a type that
    // starts with an `SEncParamExt`), and SetOption reads it back. It outlives both calls.
    unsafe {
        let mut p: SEncParamExt = std::mem::zeroed();
        if enc
            .raw_api()
            .get_option(ENCODER_OPTION_SVC_ENCODE_PARAM_EXT, (&raw mut p).cast())
            != 0
        {
            return false;
        }
        p.iMultipleThreadIdc = n.min(u32::from(u16::MAX)) as u16;
        let slices = &mut p.sSpatialLayers[0].sSliceArgument;
        slices.uiSliceMode = SM_FIXEDSLCNUM_SLICE;
        slices.uiSliceNum = n;
        enc.raw_api()
            .set_option(ENCODER_OPTION_SVC_ENCODE_PARAM_EXT, (&raw mut p).cast())
            == 0
    }
}

/// Encode the following frames at quantizer `qp` throughout, without rate control (and
/// without its frame skipping): a refinement pass over a still picture. The layer's QP
/// is a "little setting" openh264 changes in place (no restart, no keyframe), and
/// switching the rate-control mode swaps its functions but keeps its state, so
/// [`rate_controlled`] resumes where the controller left off.
pub fn fixed_qp(enc: &mut Encoder, qp: u8) -> bool {
    // SAFETY: `SEncParamExt` is what this option reads and writes; zeroed, then filled
    // by the encoder with its current parameters, so only the QP differs when set back.
    unsafe {
        let mut p: SEncParamExt = std::mem::zeroed();
        if enc
            .raw_api()
            .get_option(ENCODER_OPTION_SVC_ENCODE_PARAM_EXT, (&raw mut p).cast())
            != 0
        {
            return false;
        }
        if p.sSpatialLayers[0].iDLayerQp != c_int::from(qp) {
            p.sSpatialLayers[0].iDLayerQp = c_int::from(qp);
            if enc
                .raw_api()
                .set_option(ENCODER_OPTION_SVC_ENCODE_PARAM_EXT, (&raw mut p).cast())
                != 0
            {
                return false;
            }
        }
    }
    set_rc_mode(enc, RC_OFF_MODE)
}

/// Back to bitrate rate control after [`fixed_qp`].
pub fn rate_controlled(enc: &mut Encoder) -> bool {
    set_rc_mode(enc, RC_BITRATE_MODE)
}

fn set_rc_mode(enc: &mut Encoder, mode: RC_MODES) -> bool {
    let mut m: c_int = mode;
    // SAFETY: as in `set_rate` (the option takes an int).
    unsafe {
        enc.raw_api()
            .set_option(ENCODER_OPTION_RC_MODE, (&raw mut m).cast())
            == 0
    }
}
