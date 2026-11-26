//! The part of libva (VA-API) Rconn uses, loaded at run time with `dlopen`: the package
//! does not depend on libva, and a machine without it (or without a GPU) encodes with
//! openh264 as before.
//!
//! Types and constants are transcribed from the libva 2.17 headers (MIT). The structs'
//! sizes and the offsets of their less obvious fields are checked at compile time
//! against numbers printed by a C program built with those headers (LP64: x86_64 and
//! aarch64 Linux); the layouts have been stable since libva 2.0.
#![allow(
    unsafe_code,
    non_snake_case,
    non_camel_case_types,
    non_upper_case_globals,
    dead_code
)]

use libloading::Library;
use std::ffi::{c_char, c_int, c_uint, c_void};

pub type VADisplay = *mut c_void;
pub type VAStatus = c_int;
pub type VAConfigID = c_uint;
pub type VAContextID = c_uint;
pub type VASurfaceID = c_uint;
pub type VABufferID = c_uint;
pub type VAImageID = c_uint;
pub type VAProfile = c_int;
pub type VAEntrypoint = c_int;
pub type VABufferType = c_int;
pub type VAConfigAttribType = c_int;
pub type VASurfaceAttribType = c_int;
pub type VAGenericValueType = c_int;
pub type VAMessageCallback = Option<unsafe extern "C" fn(*mut c_void, *const c_char)>;

pub const VA_STATUS_SUCCESS: VAStatus = 0;
pub const VA_INVALID_ID: c_uint = 0xffff_ffff;
pub const VA_INVALID_SURFACE: VASurfaceID = VA_INVALID_ID;
pub const VA_PROGRESSIVE: c_int = 0x1;
pub const VA_ATTRIB_NOT_SUPPORTED: u32 = 0x8000_0000;

pub const VAProfileNone: VAProfile = -1;
pub const VAProfileH264Main: VAProfile = 6;
pub const VAProfileH264High: VAProfile = 7;
pub const VAProfileH264ConstrainedBaseline: VAProfile = 13;

pub const VAEntrypointEncSlice: VAEntrypoint = 6;
pub const VAEntrypointEncSliceLP: VAEntrypoint = 8;
pub const VAEntrypointVideoProc: VAEntrypoint = 10;

pub const VAConfigAttribRTFormat: VAConfigAttribType = 0;
pub const VAConfigAttribRateControl: VAConfigAttribType = 5;
pub const VAConfigAttribEncPackedHeaders: VAConfigAttribType = 10;
pub const VAConfigAttribEncMaxRefFrames: VAConfigAttribType = 13;

pub const VA_RT_FORMAT_YUV420: u32 = 0x0000_0001;
pub const VA_RT_FORMAT_RGB32: u32 = 0x0002_0000;

pub const VA_RC_CBR: u32 = 0x0000_0002;
pub const VA_RC_VBR: u32 = 0x0000_0004;
pub const VA_RC_CQP: u32 = 0x0000_0010;

pub const VAEncCodedBufferType: VABufferType = 21;
pub const VAEncSequenceParameterBufferType: VABufferType = 22;
pub const VAEncPictureParameterBufferType: VABufferType = 23;
pub const VAEncSliceParameterBufferType: VABufferType = 24;
pub const VAEncPackedHeaderParameterBufferType: VABufferType = 25;
pub const VAEncPackedHeaderDataBufferType: VABufferType = 26;
pub const VAEncMiscParameterBufferType: VABufferType = 27;
pub const VAProcPipelineParameterBufferType: VABufferType = 41;

pub const VA_ENC_PACKED_HEADER_SEQUENCE: u32 = 0x1;
pub const VAEncPackedHeaderSequence: u32 = 1;

pub const VAEncMiscParameterTypeFrameRate: u32 = 0;
pub const VAEncMiscParameterTypeRateControl: u32 = 1;
pub const VAEncMiscParameterTypeHRD: u32 = 5;

pub const VASurfaceAttribPixelFormat: VASurfaceAttribType = 1;
pub const VASurfaceAttribMemoryType: VASurfaceAttribType = 6;
pub const VASurfaceAttribExternalBufferDescriptor: VASurfaceAttribType = 7;
pub const VASurfaceAttribUsageHint: VASurfaceAttribType = 8;
pub const VA_SURFACE_ATTRIB_SETTABLE: u32 = 0x2;
pub const VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2: i32 = 0x4000_0000;
pub const VA_SURFACE_ATTRIB_USAGE_HINT_ENCODER: i32 = 2;
pub const VA_SURFACE_ATTRIB_USAGE_HINT_VPP_READ: i32 = 4;
pub const VA_SURFACE_ATTRIB_USAGE_HINT_VPP_WRITE: i32 = 8;

pub const VAGenericValueTypeInteger: VAGenericValueType = 1;
pub const VAGenericValueTypePointer: VAGenericValueType = 3;

pub const VA_PICTURE_H264_INVALID: u32 = 0x01;
pub const VA_PICTURE_H264_SHORT_TERM_REFERENCE: u32 = 0x08;

pub const VA_CODED_BUF_STATUS_SLICE_OVERFLOW_MASK: u32 = 0x200;

pub const VA_FOURCC_NV12: u32 = 0x3231_564E;
pub const VA_FOURCC_RGBA: u32 = 0x4142_4752;
pub const VA_FOURCC_RGBX: u32 = 0x5842_4752;
pub const VA_FOURCC_BGRA: u32 = 0x4152_4742;
pub const VA_FOURCC_BGRX: u32 = 0x5852_4742;
pub const VA_LSB_FIRST: u32 = 1;

pub const VAProcColorStandardBT601: c_int = 1;
pub const VAProcColorStandardSRGB: c_int = 8;
pub const VA_SOURCE_RANGE_REDUCED: u8 = 1;
pub const VA_SOURCE_RANGE_FULL: u8 = 2;
/// Chroma sited left and vertically centred (MPEG-2 / H.264 default for 4:2:0).
pub const VA_CHROMA_SITING_VERTICAL_CENTER_HORIZONTAL_LEFT: u8 = 0x02 | 0x04;

/// Plain C data, all-zero being a valid value (libva's own convention for these).
pub trait Plain: Copy {}

/// An all-zero `T`.
pub fn zeroed<T: Plain>() -> T {
    // SAFETY: `Plain` types are `repr(C)` structs of integers, arrays and raw pointers,
    // for which all-zero bits are valid.
    unsafe { std::mem::zeroed() }
}

/// The bytes of `v`, to hand to `vaCreateBuffer`.
pub fn bytes_of<T: Plain>(v: &T) -> &[u8] {
    // SAFETY: a `Plain` value is initialized memory of `size_of::<T>()` bytes (padding
    // included: `zeroed` starts every value all-zero and fields are only assigned).
    unsafe { std::slice::from_raw_parts((v as *const T).cast::<u8>(), size_of::<T>()) }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAConfigAttrib {
    pub type_: VAConfigAttribType,
    pub value: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union VAGenericValueUnion {
    pub i: i32,
    pub f: f32,
    pub p: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAGenericValue {
    pub type_: VAGenericValueType,
    pub value: VAGenericValueUnion,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VASurfaceAttrib {
    pub type_: VASurfaceAttribType,
    pub flags: u32,
    pub value: VAGenericValue,
}

impl VASurfaceAttrib {
    pub fn int(type_: VASurfaceAttribType, i: i32) -> Self {
        Self {
            type_,
            flags: VA_SURFACE_ATTRIB_SETTABLE,
            value: VAGenericValue {
                type_: VAGenericValueTypeInteger,
                value: VAGenericValueUnion { i },
            },
        }
    }

    pub fn ptr(type_: VASurfaceAttribType, p: *mut c_void) -> Self {
        Self {
            type_,
            flags: VA_SURFACE_ATTRIB_SETTABLE,
            value: VAGenericValue {
                type_: VAGenericValueTypePointer,
                value: VAGenericValueUnion { p },
            },
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAImageFormat {
    pub fourcc: u32,
    pub byte_order: u32,
    pub bits_per_pixel: u32,
    pub depth: u32,
    pub red_mask: u32,
    pub green_mask: u32,
    pub blue_mask: u32,
    pub alpha_mask: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAImage {
    pub image_id: VAImageID,
    pub format: VAImageFormat,
    pub buf: VABufferID,
    pub width: u16,
    pub height: u16,
    pub data_size: u32,
    pub num_planes: u32,
    pub pitches: [u32; 3],
    pub offsets: [u32; 3],
    pub num_palette_entries: i32,
    pub entry_bytes: i32,
    pub component_order: [i8; 4],
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VACodedBufferSegment {
    pub size: u32,
    pub bit_offset: u32,
    pub status: u32,
    pub reserved: u32,
    pub buf: *mut c_void,
    pub next: *mut c_void,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAPictureH264 {
    pub picture_id: VASurfaceID,
    pub frame_idx: u32,
    pub flags: u32,
    pub TopFieldOrderCnt: i32,
    pub BottomFieldOrderCnt: i32,
    pub va_reserved: [u32; 4],
}

impl VAPictureH264 {
    pub const INVALID: Self = Self {
        picture_id: VA_INVALID_SURFACE,
        frame_idx: 0,
        flags: VA_PICTURE_H264_INVALID,
        TopFieldOrderCnt: 0,
        BottomFieldOrderCnt: 0,
        va_reserved: [0; 4],
    };
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncSequenceParameterBufferH264 {
    pub seq_parameter_set_id: u8,
    pub level_idc: u8,
    pub intra_period: u32,
    pub intra_idr_period: u32,
    pub ip_period: u32,
    pub bits_per_second: u32,
    pub max_num_ref_frames: u32,
    pub picture_width_in_mbs: u16,
    pub picture_height_in_mbs: u16,
    /// Bit fields; see [`super::encode::seq_fields`].
    pub seq_fields: u32,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub num_ref_frames_in_pic_order_cnt_cycle: u8,
    pub offset_for_non_ref_pic: i32,
    pub offset_for_top_to_bottom_field: i32,
    pub offset_for_ref_frame: [i32; 256],
    pub frame_cropping_flag: u8,
    pub frame_crop_left_offset: u32,
    pub frame_crop_right_offset: u32,
    pub frame_crop_top_offset: u32,
    pub frame_crop_bottom_offset: u32,
    pub vui_parameters_present_flag: u8,
    pub vui_fields: u32,
    pub aspect_ratio_idc: u8,
    pub sar_width: u32,
    pub sar_height: u32,
    pub num_units_in_tick: u32,
    pub time_scale: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncPictureParameterBufferH264 {
    pub CurrPic: VAPictureH264,
    pub ReferenceFrames: [VAPictureH264; 16],
    pub coded_buf: VABufferID,
    pub pic_parameter_set_id: u8,
    pub seq_parameter_set_id: u8,
    pub last_picture: u8,
    pub frame_num: u16,
    pub pic_init_qp: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub chroma_qp_index_offset: i8,
    pub second_chroma_qp_index_offset: i8,
    /// Bit fields; see [`super::encode::pic_fields`].
    pub pic_fields: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncSliceParameterBufferH264 {
    pub macroblock_address: u32,
    pub num_macroblocks: u32,
    pub macroblock_info: VABufferID,
    pub slice_type: u8,
    pub pic_parameter_set_id: u8,
    pub idr_pic_id: u16,
    pub pic_order_cnt_lsb: u16,
    pub delta_pic_order_cnt_bottom: i32,
    pub delta_pic_order_cnt: [i32; 2],
    pub direct_spatial_mv_pred_flag: u8,
    pub num_ref_idx_active_override_flag: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub RefPicList0: [VAPictureH264; 32],
    pub RefPicList1: [VAPictureH264; 32],
    pub luma_log2_weight_denom: u8,
    pub chroma_log2_weight_denom: u8,
    pub luma_weight_l0_flag: u8,
    pub luma_weight_l0: [i16; 32],
    pub luma_offset_l0: [i16; 32],
    pub chroma_weight_l0_flag: u8,
    pub chroma_weight_l0: [[i16; 2]; 32],
    pub chroma_offset_l0: [[i16; 2]; 32],
    pub luma_weight_l1_flag: u8,
    pub luma_weight_l1: [i16; 32],
    pub luma_offset_l1: [i16; 32],
    pub chroma_weight_l1_flag: u8,
    pub chroma_weight_l1: [[i16; 2]; 32],
    pub chroma_offset_l1: [[i16; 2]; 32],
    pub cabac_init_idc: u8,
    pub slice_qp_delta: i8,
    pub disable_deblocking_filter_idc: u8,
    pub slice_alpha_c0_offset_div2: i8,
    pub slice_beta_offset_div2: i8,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncMiscParameterRateControl {
    pub bits_per_second: u32,
    pub target_percentage: u32,
    pub window_size: u32,
    pub initial_qp: u32,
    pub min_qp: u32,
    pub basic_unit_size: u32,
    /// Bit fields; see [`super::encode::rc_flags`].
    pub rc_flags: u32,
    pub ICQ_quality_factor: u32,
    pub max_qp: u32,
    pub quality_factor: u32,
    pub target_frame_size: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncMiscParameterFrameRate {
    /// Numerator in the low 16 bits, denominator in the high 16 (0 = 1).
    pub framerate: u32,
    pub framerate_flags: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncMiscParameterHRD {
    pub initial_buffer_fullness: u32,
    pub buffer_size: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncPackedHeaderParameterBuffer {
    pub type_: u32,
    pub bit_length: u32,
    pub has_emulation_bytes: u8,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VADRMPRIMEObject {
    pub fd: c_int,
    pub size: u32,
    pub drm_format_modifier: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VADRMPRIMELayer {
    pub drm_format: u32,
    pub num_planes: u32,
    pub object_index: [u32; 4],
    pub offset: [u32; 4],
    pub pitch: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VADRMPRIMESurfaceDescriptor {
    /// The surface's VA fourcc (`VA_FOURCC_*`).
    pub fourcc: u32,
    pub width: u32,
    pub height: u32,
    pub num_objects: u32,
    pub objects: [VADRMPRIMEObject; 4],
    pub num_layers: u32,
    pub layers: [VADRMPRIMELayer; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VARectangle {
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAProcColorProperties {
    pub chroma_sample_location: u8,
    pub color_range: u8,
    pub colour_primaries: u8,
    pub transfer_characteristics: u8,
    pub matrix_coefficients: u8,
    pub reserved: [u8; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAProcPipelineParameterBuffer {
    pub surface: VASurfaceID,
    pub surface_region: *const VARectangle,
    pub surface_color_standard: c_int,
    pub output_region: *const VARectangle,
    pub output_background_color: u32,
    pub output_color_standard: c_int,
    pub pipeline_flags: u32,
    pub filter_flags: u32,
    pub filters: *mut VABufferID,
    pub num_filters: u32,
    pub forward_references: *mut VASurfaceID,
    pub num_forward_references: u32,
    pub backward_references: *mut VASurfaceID,
    pub num_backward_references: u32,
    pub rotation_state: u32,
    pub blend_state: *const c_void,
    pub mirror_state: u32,
    pub additional_outputs: *mut VASurfaceID,
    pub num_additional_outputs: u32,
    pub input_surface_flag: u32,
    pub output_surface_flag: u32,
    pub input_color_properties: VAProcColorProperties,
    pub output_color_properties: VAProcColorProperties,
    pub processing_mode: c_int,
    pub output_hdr_metadata: *mut c_void,
    pub va_reserved: [u32; 16],
}

impl Plain for VAConfigAttrib {}
impl Plain for VAImageFormat {}
impl Plain for VAImage {}
impl Plain for VAPictureH264 {}
impl Plain for VAEncSequenceParameterBufferH264 {}
impl Plain for VAEncPictureParameterBufferH264 {}
impl Plain for VAEncSliceParameterBufferH264 {}
impl Plain for VAEncMiscParameterRateControl {}
impl Plain for VAEncMiscParameterFrameRate {}
impl Plain for VAEncMiscParameterHRD {}
impl Plain for VAEncPackedHeaderParameterBuffer {}
impl Plain for VADRMPRIMESurfaceDescriptor {}
impl Plain for VAProcPipelineParameterBuffer {}

// The layouts of the libva 2.17 headers (sizes and offsets printed by a C program).
#[cfg(target_pointer_width = "64")]
const _: () = {
    use std::mem::offset_of;
    assert!(size_of::<VAConfigAttrib>() == 8);
    assert!(size_of::<VAGenericValue>() == 16);
    assert!(size_of::<VASurfaceAttrib>() == 24);
    assert!(offset_of!(VASurfaceAttrib, value) == 8);
    assert!(size_of::<VAImageFormat>() == 48);
    assert!(size_of::<VAImage>() == 120);
    assert!(offset_of!(VAImage, buf) == 52);
    assert!(offset_of!(VAImage, width) == 56);
    assert!(offset_of!(VAImage, data_size) == 60);
    assert!(offset_of!(VAImage, pitches) == 68);
    assert!(offset_of!(VAImage, offsets) == 80);
    assert!(offset_of!(VAImage, component_order) == 100);
    assert!(size_of::<VACodedBufferSegment>() == 48);
    assert!(offset_of!(VACodedBufferSegment, buf) == 16);
    assert!(offset_of!(VACodedBufferSegment, next) == 24);
    assert!(size_of::<VAPictureH264>() == 36);
    assert!(size_of::<VAEncSequenceParameterBufferH264>() == 1132);
    assert!(offset_of!(VAEncSequenceParameterBufferH264, seq_fields) == 28);
    assert!(offset_of!(VAEncSequenceParameterBufferH264, offset_for_non_ref_pic) == 36);
    assert!(offset_of!(VAEncSequenceParameterBufferH264, frame_cropping_flag) == 1068);
    assert!(offset_of!(VAEncSequenceParameterBufferH264, frame_crop_left_offset) == 1072);
    assert!(offset_of!(VAEncSequenceParameterBufferH264, vui_fields) == 1092);
    assert!(offset_of!(VAEncSequenceParameterBufferH264, time_scale) == 1112);
    assert!(size_of::<VAEncPictureParameterBufferH264>() == 648);
    assert!(offset_of!(VAEncPictureParameterBufferH264, coded_buf) == 612);
    assert!(offset_of!(VAEncPictureParameterBufferH264, frame_num) == 620);
    assert!(offset_of!(VAEncPictureParameterBufferH264, pic_init_qp) == 622);
    assert!(offset_of!(VAEncPictureParameterBufferH264, chroma_qp_index_offset) == 625);
    assert!(offset_of!(VAEncPictureParameterBufferH264, pic_fields) == 628);
    assert!(size_of::<VAEncSliceParameterBufferH264>() == 3140);
    assert!(offset_of!(VAEncSliceParameterBufferH264, slice_type) == 12);
    assert!(offset_of!(VAEncSliceParameterBufferH264, idr_pic_id) == 14);
    assert!(offset_of!(VAEncSliceParameterBufferH264, delta_pic_order_cnt_bottom) == 20);
    assert!(offset_of!(VAEncSliceParameterBufferH264, direct_spatial_mv_pred_flag) == 32);
    assert!(offset_of!(VAEncSliceParameterBufferH264, RefPicList0) == 36);
    assert!(offset_of!(VAEncSliceParameterBufferH264, luma_log2_weight_denom) == 2340);
    assert!(offset_of!(VAEncSliceParameterBufferH264, luma_weight_l0) == 2344);
    assert!(offset_of!(VAEncSliceParameterBufferH264, chroma_weight_l0) == 2474);
    assert!(offset_of!(VAEncSliceParameterBufferH264, luma_weight_l1_flag) == 2730);
    assert!(offset_of!(VAEncSliceParameterBufferH264, cabac_init_idc) == 3118);
    assert!(offset_of!(VAEncSliceParameterBufferH264, va_reserved) == 3124);
    assert!(size_of::<VAEncMiscParameterRateControl>() == 60);
    assert!(size_of::<VAEncMiscParameterFrameRate>() == 24);
    assert!(size_of::<VAEncMiscParameterHRD>() == 24);
    assert!(size_of::<VAEncPackedHeaderParameterBuffer>() == 28);
    assert!(size_of::<VADRMPRIMESurfaceDescriptor>() == 312);
    assert!(offset_of!(VADRMPRIMESurfaceDescriptor, objects) == 16);
    assert!(offset_of!(VADRMPRIMESurfaceDescriptor, num_layers) == 80);
    assert!(offset_of!(VADRMPRIMESurfaceDescriptor, layers) == 84);
    assert!(size_of::<VARectangle>() == 8);
    assert!(size_of::<VAProcColorProperties>() == 8);
    assert!(size_of::<VAProcPipelineParameterBuffer>() == 224);
    assert!(offset_of!(VAProcPipelineParameterBuffer, surface_region) == 8);
    assert!(offset_of!(VAProcPipelineParameterBuffer, surface_color_standard) == 16);
    assert!(offset_of!(VAProcPipelineParameterBuffer, output_background_color) == 32);
    assert!(offset_of!(VAProcPipelineParameterBuffer, filters) == 48);
    assert!(offset_of!(VAProcPipelineParameterBuffer, forward_references) == 64);
    assert!(offset_of!(VAProcPipelineParameterBuffer, rotation_state) == 92);
    assert!(offset_of!(VAProcPipelineParameterBuffer, blend_state) == 96);
    assert!(offset_of!(VAProcPipelineParameterBuffer, additional_outputs) == 112);
    assert!(offset_of!(VAProcPipelineParameterBuffer, input_surface_flag) == 124);
    assert!(offset_of!(VAProcPipelineParameterBuffer, input_color_properties) == 132);
    assert!(offset_of!(VAProcPipelineParameterBuffer, processing_mode) == 148);
    assert!(offset_of!(VAProcPipelineParameterBuffer, output_hdr_metadata) == 152);
    assert!(offset_of!(VAProcPipelineParameterBuffer, va_reserved) == 160);
};

/// Declares the libva entry points as fields of [`Va`] and loads them by name.
macro_rules! entry_points {
    ($($lib:ident: $name:ident($($arg:ty),*) -> $ret:ty;)*) => {
        /// libva's entry points (from `libva.so.2` and `libva-drm.so.2`), valid while the
        /// libraries in this struct stay loaded.
        pub struct Va {
            _va: Library,
            _drm: Library,
            $(pub $name: unsafe extern "C" fn($($arg),*) -> $ret,)*
            /// Absent before libva 2.0; libva's messages then go to stderr.
            pub vaSetErrorCallback: Option<unsafe extern "C" fn(VADisplay, VAMessageCallback, *mut c_void) -> VAMessageCallback>,
            pub vaSetInfoCallback: Option<unsafe extern "C" fn(VADisplay, VAMessageCallback, *mut c_void) -> VAMessageCallback>,
        }

        impl Va {
            /// Load libva; `Err` says what is missing.
            pub fn load() -> Result<Self, String> {
                // SAFETY: libva and libva-drm are C libraries without initializers that
                // assume anything of the process.
                let va = unsafe { Library::new("libva.so.2") }
                    .map_err(|e| format!("libva.so.2 not found ({e})"))?;
                let drm = unsafe { Library::new("libva-drm.so.2") }
                    .map_err(|e| format!("libva-drm.so.2 not found ({e})"))?;
                $(
                    let $name = {
                        let lib = match stringify!($lib) { "drm" => &drm, _ => &va };
                        // SAFETY: the symbol has this C signature in libva 2.x.
                        *unsafe {
                            lib.get::<unsafe extern "C" fn($($arg),*) -> $ret>(
                                concat!(stringify!($name), "\0").as_bytes(),
                            )
                        }
                        .map_err(|e| format!("libva lacks {}: {e}", stringify!($name)))?
                    };
                )*
                // SAFETY: as above.
                let vaSetErrorCallback = unsafe { va.get(b"vaSetErrorCallback\0") }.ok().map(|s| *s);
                let vaSetInfoCallback = unsafe { va.get(b"vaSetInfoCallback\0") }.ok().map(|s| *s);
                Ok(Self { _va: va, _drm: drm, $($name,)* vaSetErrorCallback, vaSetInfoCallback })
            }
        }
    };
}

entry_points! {
    drm: vaGetDisplayDRM(c_int) -> VADisplay;
    va: vaInitialize(VADisplay, *mut c_int, *mut c_int) -> VAStatus;
    va: vaTerminate(VADisplay) -> VAStatus;
    va: vaErrorStr(VAStatus) -> *const c_char;
    va: vaQueryVendorString(VADisplay) -> *const c_char;
    va: vaMaxNumProfiles(VADisplay) -> c_int;
    va: vaMaxNumEntrypoints(VADisplay) -> c_int;
    va: vaMaxNumImageFormats(VADisplay) -> c_int;
    va: vaQueryConfigProfiles(VADisplay, *mut VAProfile, *mut c_int) -> VAStatus;
    va: vaQueryConfigEntrypoints(VADisplay, VAProfile, *mut VAEntrypoint, *mut c_int) -> VAStatus;
    va: vaQueryImageFormats(VADisplay, *mut VAImageFormat, *mut c_int) -> VAStatus;
    va: vaGetConfigAttributes(VADisplay, VAProfile, VAEntrypoint, *mut VAConfigAttrib, c_int) -> VAStatus;
    va: vaCreateConfig(VADisplay, VAProfile, VAEntrypoint, *mut VAConfigAttrib, c_int, *mut VAConfigID) -> VAStatus;
    va: vaDestroyConfig(VADisplay, VAConfigID) -> VAStatus;
    va: vaCreateSurfaces(VADisplay, c_uint, c_uint, c_uint, *mut VASurfaceID, c_uint, *mut VASurfaceAttrib, c_uint) -> VAStatus;
    va: vaDestroySurfaces(VADisplay, *mut VASurfaceID, c_int) -> VAStatus;
    va: vaCreateContext(VADisplay, VAConfigID, c_int, c_int, c_int, *mut VASurfaceID, c_int, *mut VAContextID) -> VAStatus;
    va: vaDestroyContext(VADisplay, VAContextID) -> VAStatus;
    va: vaCreateBuffer(VADisplay, VAContextID, VABufferType, c_uint, c_uint, *mut c_void, *mut VABufferID) -> VAStatus;
    va: vaDestroyBuffer(VADisplay, VABufferID) -> VAStatus;
    va: vaMapBuffer(VADisplay, VABufferID, *mut *mut c_void) -> VAStatus;
    va: vaUnmapBuffer(VADisplay, VABufferID) -> VAStatus;
    va: vaBeginPicture(VADisplay, VAContextID, VASurfaceID) -> VAStatus;
    va: vaRenderPicture(VADisplay, VAContextID, *mut VABufferID, c_int) -> VAStatus;
    va: vaEndPicture(VADisplay, VAContextID) -> VAStatus;
    va: vaSyncSurface(VADisplay, VASurfaceID) -> VAStatus;
    va: vaCreateImage(VADisplay, *mut VAImageFormat, c_int, c_int, *mut VAImage) -> VAStatus;
    va: vaDestroyImage(VADisplay, VAImageID) -> VAStatus;
    va: vaPutImage(VADisplay, VASurfaceID, VAImageID, c_int, c_int, c_uint, c_uint, c_int, c_int, c_uint, c_uint) -> VAStatus;
    va: vaGetImage(VADisplay, VASurfaceID, c_int, c_int, c_uint, c_uint, VAImageID) -> VAStatus;
}
