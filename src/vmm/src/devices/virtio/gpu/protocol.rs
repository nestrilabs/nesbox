// Copyright 2019 The ChromiumOS Authors – ported to Firecracker 2024
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
//
// Virtio GPU wire protocol definitions and codec.
// krun_display references have been removed; display operations that are no
// longer reachable are kept as stubs so the rest of the logic compiles
// unchanged.

#![allow(dead_code)]
#![allow(non_camel_case_types)]

use std::cmp::min;
use std::fmt;
use std::io::Write;
use std::marker::PhantomData;
use std::mem::{size_of, size_of_val};
use std::str::from_utf8;

use rutabaga_gfx::RutabagaError;
use thiserror::Error;
use vm_memory::ByteValued;

use super::descriptor_utils::{Reader, Writer};

// ---------------------------------------------------------------------------
// Command type constants
// ---------------------------------------------------------------------------

pub const VIRTIO_GPU_UNDEFINED: u32 = 0x0;

/* 2-D commands */
pub const VIRTIO_GPU_CMD_GET_DISPLAY_INFO: u32 = 0x100;
pub const VIRTIO_GPU_CMD_RESOURCE_CREATE_2D: u32 = 0x101;
pub const VIRTIO_GPU_CMD_RESOURCE_UNREF: u32 = 0x102;
pub const VIRTIO_GPU_CMD_SET_SCANOUT: u32 = 0x103;
pub const VIRTIO_GPU_CMD_RESOURCE_FLUSH: u32 = 0x104;
pub const VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D: u32 = 0x105;
pub const VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING: u32 = 0x106;
pub const VIRTIO_GPU_CMD_RESOURCE_DETACH_BACKING: u32 = 0x107;
pub const VIRTIO_GPU_CMD_GET_CAPSET_INFO: u32 = 0x108;
pub const VIRTIO_GPU_CMD_GET_CAPSET: u32 = 0x109;
pub const VIRTIO_GPU_CMD_GET_EDID: u32 = 0x10a;
pub const VIRTIO_GPU_CMD_RESOURCE_ASSIGN_UUID: u32 = 0x10b;
pub const VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB: u32 = 0x10c;
pub const VIRTIO_GPU_CMD_SET_SCANOUT_BLOB: u32 = 0x10d;

/* 3-D commands */
pub const VIRTIO_GPU_CMD_CTX_CREATE: u32 = 0x200;
pub const VIRTIO_GPU_CMD_CTX_DESTROY: u32 = 0x201;
pub const VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE: u32 = 0x202;
pub const VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE: u32 = 0x203;
pub const VIRTIO_GPU_CMD_RESOURCE_CREATE_3D: u32 = 0x204;
pub const VIRTIO_GPU_CMD_TRANSFER_TO_HOST_3D: u32 = 0x205;
pub const VIRTIO_GPU_CMD_TRANSFER_FROM_HOST_3D: u32 = 0x206;
pub const VIRTIO_GPU_CMD_SUBMIT_3D: u32 = 0x207;
pub const VIRTIO_GPU_CMD_RESOURCE_MAP_BLOB: u32 = 0x208;
pub const VIRTIO_GPU_CMD_RESOURCE_UNMAP_BLOB: u32 = 0x209;

/* cursor commands */
pub const VIRTIO_GPU_CMD_UPDATE_CURSOR: u32 = 0x300;
pub const VIRTIO_GPU_CMD_MOVE_CURSOR: u32 = 0x301;

/* success responses */
pub const VIRTIO_GPU_RESP_OK_NODATA: u32 = 0x1100;
pub const VIRTIO_GPU_RESP_OK_DISPLAY_INFO: u32 = 0x1101;
pub const VIRTIO_GPU_RESP_OK_CAPSET_INFO: u32 = 0x1102;
pub const VIRTIO_GPU_RESP_OK_CAPSET: u32 = 0x1103;
pub const VIRTIO_GPU_RESP_OK_EDID: u32 = 0x1104;
pub const VIRTIO_GPU_RESP_OK_RESOURCE_UUID: u32 = 0x1105;
pub const VIRTIO_GPU_RESP_OK_MAP_INFO: u32 = 0x1106;
/* CHROMIUM(b/277982577) */
pub const VIRTIO_GPU_RESP_OK_RESOURCE_PLANE_INFO: u32 = 0x11FF;

/* error responses */
pub const VIRTIO_GPU_RESP_ERR_UNSPEC: u32 = 0x1200;
pub const VIRTIO_GPU_RESP_ERR_OUT_OF_MEMORY: u32 = 0x1201;
pub const VIRTIO_GPU_RESP_ERR_INVALID_SCANOUT_ID: u32 = 0x1202;
pub const VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID: u32 = 0x1203;
pub const VIRTIO_GPU_RESP_ERR_INVALID_CONTEXT_ID: u32 = 0x1204;
pub const VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER: u32 = 0x1205;

/* blob memory / flag constants */
pub const VIRTIO_GPU_BLOB_MEM_GUEST: u32 = 0x0001;
pub const VIRTIO_GPU_BLOB_MEM_HOST3D: u32 = 0x0002;
pub const VIRTIO_GPU_BLOB_MEM_HOST3D_GUEST: u32 = 0x0003;

pub const VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE: u32 = 0x0001;
pub const VIRTIO_GPU_BLOB_FLAG_USE_SHAREABLE: u32 = 0x0002;
pub const VIRTIO_GPU_BLOB_FLAG_USE_CROSS_DEVICE: u32 = 0x0004;
pub const VIRTIO_GPU_BLOB_FLAG_CREATE_GUEST_HANDLE: u32 = 0x0008;

pub const VIRTIO_GPU_SHM_ID_NONE: u8 = 0x00;
pub const VIRTIO_GPU_SHM_ID_HOST_VISIBLE: u8 = 0x01;

pub const VIRTIO_GPU_FLAG_FENCE: u32 = 1 << 0;
pub const VIRTIO_GPU_FLAG_INFO_RING_IDX: u32 = 1 << 1;

// ---------------------------------------------------------------------------
// Human-readable command name helper
// ---------------------------------------------------------------------------

pub fn virtio_gpu_cmd_str(cmd: u32) -> &'static str {
    match cmd {
        VIRTIO_GPU_CMD_GET_DISPLAY_INFO => "VIRTIO_GPU_CMD_GET_DISPLAY_INFO",
        VIRTIO_GPU_CMD_RESOURCE_CREATE_2D => "VIRTIO_GPU_CMD_RESOURCE_CREATE_2D",
        VIRTIO_GPU_CMD_RESOURCE_UNREF => "VIRTIO_GPU_CMD_RESOURCE_UNREF",
        VIRTIO_GPU_CMD_SET_SCANOUT => "VIRTIO_GPU_CMD_SET_SCANOUT",
        VIRTIO_GPU_CMD_SET_SCANOUT_BLOB => "VIRTIO_GPU_CMD_SET_SCANOUT_BLOB",
        VIRTIO_GPU_CMD_RESOURCE_FLUSH => "VIRTIO_GPU_CMD_RESOURCE_FLUSH",
        VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D => "VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D",
        VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING => "VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING",
        VIRTIO_GPU_CMD_RESOURCE_DETACH_BACKING => "VIRTIO_GPU_CMD_RESOURCE_DETACH_BACKING",
        VIRTIO_GPU_CMD_GET_CAPSET_INFO => "VIRTIO_GPU_CMD_GET_CAPSET_INFO",
        VIRTIO_GPU_CMD_GET_CAPSET => "VIRTIO_GPU_CMD_GET_CAPSET",
        VIRTIO_GPU_CMD_CTX_CREATE => "VIRTIO_GPU_CMD_CTX_CREATE",
        VIRTIO_GPU_CMD_CTX_DESTROY => "VIRTIO_GPU_CMD_CTX_DESTROY",
        VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE => "VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE",
        VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE => "VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE",
        VIRTIO_GPU_CMD_RESOURCE_ASSIGN_UUID => "VIRTIO_GPU_CMD_RESOURCE_ASSIGN_UUID",
        VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB => "VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB",
        VIRTIO_GPU_CMD_RESOURCE_CREATE_3D => "VIRTIO_GPU_CMD_RESOURCE_CREATE_3D",
        VIRTIO_GPU_CMD_TRANSFER_TO_HOST_3D => "VIRTIO_GPU_CMD_TRANSFER_TO_HOST_3D",
        VIRTIO_GPU_CMD_TRANSFER_FROM_HOST_3D => "VIRTIO_GPU_CMD_TRANSFER_FROM_HOST_3D",
        VIRTIO_GPU_CMD_SUBMIT_3D => "VIRTIO_GPU_CMD_SUBMIT_3D",
        VIRTIO_GPU_CMD_RESOURCE_MAP_BLOB => "VIRTIO_GPU_RESOURCE_MAP_BLOB",
        VIRTIO_GPU_CMD_RESOURCE_UNMAP_BLOB => "VIRTIO_GPU_RESOURCE_UNMAP_BLOB",
        VIRTIO_GPU_CMD_UPDATE_CURSOR => "VIRTIO_GPU_CMD_UPDATE_CURSOR",
        VIRTIO_GPU_CMD_MOVE_CURSOR => "VIRTIO_GPU_CMD_MOVE_CURSOR",
        VIRTIO_GPU_RESP_OK_NODATA => "VIRTIO_GPU_RESP_OK_NODATA",
        VIRTIO_GPU_RESP_OK_DISPLAY_INFO => "VIRTIO_GPU_RESP_OK_DISPLAY_INFO",
        VIRTIO_GPU_RESP_OK_CAPSET_INFO => "VIRTIO_GPU_RESP_OK_CAPSET_INFO",
        VIRTIO_GPU_RESP_OK_CAPSET => "VIRTIO_GPU_RESP_OK_CAPSET",
        VIRTIO_GPU_RESP_OK_RESOURCE_PLANE_INFO => "VIRTIO_GPU_RESP_OK_RESOURCE_PLANE_INFO",
        VIRTIO_GPU_RESP_OK_RESOURCE_UUID => "VIRTIO_GPU_RESP_OK_RESOURCE_UUID",
        VIRTIO_GPU_RESP_OK_MAP_INFO => "VIRTIO_GPU_RESP_OK_MAP_INFO",
        VIRTIO_GPU_RESP_ERR_UNSPEC => "VIRTIO_GPU_RESP_ERR_UNSPEC",
        VIRTIO_GPU_RESP_ERR_OUT_OF_MEMORY => "VIRTIO_GPU_RESP_ERR_OUT_OF_MEMORY",
        VIRTIO_GPU_RESP_ERR_INVALID_SCANOUT_ID => "VIRTIO_GPU_RESP_ERR_INVALID_SCANOUT_ID",
        VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID => "VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID",
        VIRTIO_GPU_RESP_ERR_INVALID_CONTEXT_ID => "VIRTIO_GPU_RESP_ERR_INVALID_CONTEXT_ID",
        VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER => "VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER",
        _ => "UNKNOWN",
    }
}

// ---------------------------------------------------------------------------
// Wire structs
// ---------------------------------------------------------------------------

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct virtio_gpu_ctrl_hdr {
    pub type_: u32,
    pub flags: u32,
    pub fence_id: u64,
    pub ctx_id: u32,
    pub ring_idx: u8,
    pub padding: [u8; 3],
}
unsafe impl ByteValued for virtio_gpu_ctrl_hdr {}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct virtio_gpu_cursor_pos {
    pub scanout_id: u32,
    pub x: u32,
    pub y: u32,
    pub padding: u32,
}
unsafe impl ByteValued for virtio_gpu_cursor_pos {}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct virtio_gpu_update_cursor {
    pub pos: virtio_gpu_cursor_pos,
    pub resource_id: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    pub padding: u32,
}
unsafe impl ByteValued for virtio_gpu_update_cursor {}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct virtio_gpu_rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}
unsafe impl ByteValued for virtio_gpu_rect {}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct virtio_gpu_get_edid {
    pub scanout: u32,
    pub padding: u32,
}
unsafe impl ByteValued for virtio_gpu_get_edid {}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct virtio_gpu_resource_unref {
    pub resource_id: u32,
    pub padding: u32,
}
unsafe impl ByteValued for virtio_gpu_resource_unref {}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct virtio_gpu_resource_create_2d {
    pub resource_id: u32,
    pub format: u32,
    pub width: u32,
    pub height: u32,
}
unsafe impl ByteValued for virtio_gpu_resource_create_2d {}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct virtio_gpu_set_scanout {
    pub r: virtio_gpu_rect,
    pub scanout_id: u32,
    pub resource_id: u32,
}
unsafe impl ByteValued for virtio_gpu_set_scanout {}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct virtio_gpu_resource_flush {
    pub r: virtio_gpu_rect,
    pub resource_id: u32,
    pub padding: u32,
}
unsafe impl ByteValued for virtio_gpu_resource_flush {}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct virtio_gpu_transfer_to_host_2d {
    pub r: virtio_gpu_rect,
    pub offset: u64,
    pub resource_id: u32,
    pub padding: u32,
}
unsafe impl ByteValued for virtio_gpu_transfer_to_host_2d {}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct virtio_gpu_mem_entry {
    pub addr: u64,
    pub length: u32,
    pub padding: u32,
}
unsafe impl ByteValued for virtio_gpu_mem_entry {}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct virtio_gpu_resource_attach_backing {
    pub resource_id: u32,
    pub nr_entries: u32,
}
unsafe impl ByteValued for virtio_gpu_resource_attach_backing {}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct virtio_gpu_resource_detach_backing {
    pub resource_id: u32,
    pub padding: u32,
}
unsafe impl ByteValued for virtio_gpu_resource_detach_backing {}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct virtio_gpu_display_one {
    pub r: virtio_gpu_rect,
    pub enabled: u32,
    pub flags: u32,
}
unsafe impl ByteValued for virtio_gpu_display_one {}

pub const VIRTIO_GPU_MAX_SCANOUTS: u32 = 16;

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct virtio_gpu_resp_display_info {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub pmodes: [virtio_gpu_display_one; VIRTIO_GPU_MAX_SCANOUTS as usize],
}
unsafe impl ByteValued for virtio_gpu_resp_display_info {}

const EDID_BLOB_MAX_SIZE: usize = 1024;

#[derive(Debug, Copy, Clone)]
#[repr(C)]
pub struct virtio_gpu_resp_edid {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub size: u32,
    pub padding: u32,
    pub edid: [u8; EDID_BLOB_MAX_SIZE],
}
unsafe impl ByteValued for virtio_gpu_resp_edid {}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct virtio_gpu_box {
    pub x: u32,
    pub y: u32,
    pub z: u32,
    pub w: u32,
    pub h: u32,
    pub d: u32,
}
unsafe impl ByteValued for virtio_gpu_box {}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct virtio_gpu_transfer_host_3d {
    pub box_: virtio_gpu_box,
    pub offset: u64,
    pub resource_id: u32,
    pub level: u32,
    pub stride: u32,
    pub layer_stride: u32,
}
unsafe impl ByteValued for virtio_gpu_transfer_host_3d {}

pub const VIRTIO_GPU_RESOURCE_FLAG_Y_0_TOP: u32 = 1 << 0;

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct virtio_gpu_resource_create_3d {
    pub resource_id: u32,
    pub target: u32,
    pub format: u32,
    pub bind: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub array_size: u32,
    pub last_level: u32,
    pub nr_samples: u32,
    pub flags: u32,
    pub padding: u32,
}
unsafe impl ByteValued for virtio_gpu_resource_create_3d {}

pub const VIRTIO_GPU_CONTEXT_INIT_CAPSET_ID_MASK: u32 = 1 << 0;

#[derive(Copy, Clone)]
#[repr(C)]
pub struct virtio_gpu_ctx_create {
    pub nlen: u32,
    pub context_init: u32,
    pub debug_name: [u8; 64],
}
unsafe impl ByteValued for virtio_gpu_ctx_create {}

impl Default for virtio_gpu_ctx_create {
    fn default() -> Self {
        // SAFETY: All-zero bit pattern is valid for this POD.
        unsafe { std::mem::zeroed() }
    }
}
impl fmt::Debug for virtio_gpu_ctx_create {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = min(64, self.nlen as usize);
        let name = from_utf8(&self.debug_name[..n]).unwrap_or("<invalid>");
        f.debug_struct("virtio_gpu_ctx_create")
            .field("debug_name", &name)
            .finish()
    }
}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct virtio_gpu_ctx_destroy {}
unsafe impl ByteValued for virtio_gpu_ctx_destroy {}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct virtio_gpu_ctx_resource {
    pub resource_id: u32,
    pub padding: u32,
}
unsafe impl ByteValued for virtio_gpu_ctx_resource {}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct virtio_gpu_cmd_submit {
    pub size: u32,
    pub num_in_fences: u32,
}
unsafe impl ByteValued for virtio_gpu_cmd_submit {}

pub const VIRTIO_GPU_CAPSET_VIRGL: u32 = 1;
pub const VIRTIO_GPU_CAPSET_VIRGL2: u32 = 2;
pub const VIRTIO_GPU_CAPSET_GFXSTREAM: u32 = 3;
pub const VIRTIO_GPU_CAPSET_VENUS: u32 = 4;
pub const VIRTIO_GPU_CAPSET_CROSS_DOMAIN: u32 = 5;

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct virtio_gpu_get_capset_info {
    pub capset_index: u32,
    pub padding: u32,
}
unsafe impl ByteValued for virtio_gpu_get_capset_info {}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct virtio_gpu_resp_capset_info {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub capset_id: u32,
    pub capset_max_version: u32,
    pub capset_max_size: u32,
    pub padding: u32,
}
unsafe impl ByteValued for virtio_gpu_resp_capset_info {}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct virtio_gpu_get_capset {
    pub capset_id: u32,
    pub capset_version: u32,
}
unsafe impl ByteValued for virtio_gpu_get_capset {}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct virtio_gpu_resp_capset {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub capset_data: PhantomData<[u8]>,
}
unsafe impl ByteValued for virtio_gpu_resp_capset {}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct virtio_gpu_resp_resource_plane_info {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub count: u32,
    pub padding: u32,
    pub format_modifier: u64,
    pub strides: [u32; 4],
    pub offsets: [u32; 4],
}
unsafe impl ByteValued for virtio_gpu_resp_resource_plane_info {}

pub const PLANE_INFO_MAX_COUNT: usize = 4;
pub const VIRTIO_GPU_EVENT_DISPLAY: u32 = 1 << 0;

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct virtio_gpu_resource_create_blob {
    pub resource_id: u32,
    pub blob_mem: u32,
    pub blob_flags: u32,
    pub nr_entries: u32,
    pub blob_id: u64,
    pub size: u64,
}
unsafe impl ByteValued for virtio_gpu_resource_create_blob {}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct virtio_gpu_resource_map_blob {
    pub resource_id: u32,
    pub padding: u32,
    pub offset: u64,
}
unsafe impl ByteValued for virtio_gpu_resource_map_blob {}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct virtio_gpu_resource_unmap_blob {
    pub resource_id: u32,
    pub padding: u32,
}
unsafe impl ByteValued for virtio_gpu_resource_unmap_blob {}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct virtio_gpu_resp_map_info {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub map_info: u32,
    pub padding: u32,
}
unsafe impl ByteValued for virtio_gpu_resp_map_info {}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct virtio_gpu_resource_assign_uuid {
    pub resource_id: u32,
    pub padding: u32,
}
unsafe impl ByteValued for virtio_gpu_resource_assign_uuid {}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct virtio_gpu_resp_resource_uuid {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub uuid: [u8; 16],
}
unsafe impl ByteValued for virtio_gpu_resp_resource_uuid {}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct virtio_gpu_set_scanout_blob {
    pub r: virtio_gpu_rect,
    pub scanout_id: u32,
    pub resource_id: u32,
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub padding: u32,
    pub strides: [u32; 4],
    pub offsets: [u32; 4],
}
unsafe impl ByteValued for virtio_gpu_set_scanout_blob {}

/* pixel formats */
pub const VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM: u32 = 1;
pub const VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM: u32 = 2;
pub const VIRTIO_GPU_FORMAT_A8R8G8B8_UNORM: u32 = 3;
pub const VIRTIO_GPU_FORMAT_X8R8G8B8_UNORM: u32 = 4;
pub const VIRTIO_GPU_FORMAT_R8G8B8A8_UNORM: u32 = 67;
pub const VIRTIO_GPU_FORMAT_X8B8G8R8_UNORM: u32 = 68;
pub const VIRTIO_GPU_FORMAT_A8B8G8R8_UNORM: u32 = 121;
pub const VIRTIO_GPU_FORMAT_R8G8B8X8_UNORM: u32 = 134;

// ---------------------------------------------------------------------------
// GpuCommand enum
// ---------------------------------------------------------------------------

#[derive(Copy, Clone)]
pub enum GpuCommand {
    GetDisplayInfo,
    GetEdid(virtio_gpu_get_edid),
    ResourceCreate2d(virtio_gpu_resource_create_2d),
    ResourceUnref(virtio_gpu_resource_unref),
    SetScanout(virtio_gpu_set_scanout),
    SetScanoutBlob(virtio_gpu_set_scanout_blob),
    ResourceFlush(virtio_gpu_resource_flush),
    TransferToHost2d(virtio_gpu_transfer_to_host_2d),
    ResourceAttachBacking(virtio_gpu_resource_attach_backing),
    ResourceDetachBacking(virtio_gpu_resource_detach_backing),
    GetCapsetInfo(virtio_gpu_get_capset_info),
    GetCapset(virtio_gpu_get_capset),
    CtxCreate(virtio_gpu_ctx_create),
    CtxDestroy(virtio_gpu_ctx_destroy),
    CtxAttachResource(virtio_gpu_ctx_resource),
    CtxDetachResource(virtio_gpu_ctx_resource),
    ResourceCreate3d(virtio_gpu_resource_create_3d),
    TransferToHost3d(virtio_gpu_transfer_host_3d),
    TransferFromHost3d(virtio_gpu_transfer_host_3d),
    CmdSubmit3d(virtio_gpu_cmd_submit),
    ResourceCreateBlob(virtio_gpu_resource_create_blob),
    ResourceMapBlob(virtio_gpu_resource_map_blob),
    ResourceUnmapBlob(virtio_gpu_resource_unmap_blob),
    UpdateCursor(virtio_gpu_update_cursor),
    MoveCursor(virtio_gpu_update_cursor),
    ResourceAssignUuid(virtio_gpu_resource_assign_uuid),
}

#[derive(Error, Debug)]
pub enum GpuCommandDecodeError {
    #[error("invalid command type ({0})")]
    InvalidType(u32),
    #[error("I/O error: {0}")]
    IO(std::io::Error),
}

impl From<std::io::Error> for GpuCommandDecodeError {
    fn from(e: std::io::Error) -> Self {
        GpuCommandDecodeError::IO(e)
    }
}

impl fmt::Debug for GpuCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use GpuCommand::*;
        let name = match self {
            GetDisplayInfo => "GetDisplayInfo",
            GetEdid(_) => "GetEdid",
            ResourceCreate2d(_) => "ResourceCreate2d",
            ResourceUnref(_) => "ResourceUnref",
            SetScanout(_) => "SetScanout",
            SetScanoutBlob(_) => "SetScanoutBlob",
            ResourceFlush(_) => "ResourceFlush",
            TransferToHost2d(_) => "TransferToHost2d",
            ResourceAttachBacking(_) => "ResourceAttachBacking",
            ResourceDetachBacking(_) => "ResourceDetachBacking",
            GetCapsetInfo(_) => "GetCapsetInfo",
            GetCapset(_) => "GetCapset",
            CtxCreate(_) => "CtxCreate",
            CtxDestroy(_) => "CtxDestroy",
            CtxAttachResource(_) => "CtxAttachResource",
            CtxDetachResource(_) => "CtxDetachResource",
            ResourceCreate3d(_) => "ResourceCreate3d",
            TransferToHost3d(_) => "TransferToHost3d",
            TransferFromHost3d(_) => "TransferFromHost3d",
            CmdSubmit3d(_) => "CmdSubmit3d",
            ResourceCreateBlob(_) => "ResourceCreateBlob",
            ResourceMapBlob(_) => "ResourceMapBlob",
            ResourceUnmapBlob(_) => "ResourceUnmapBlob",
            UpdateCursor(_) => "UpdateCursor",
            MoveCursor(_) => "MoveCursor",
            ResourceAssignUuid(_) => "ResourceAssignUuid",
        };
        f.debug_struct(name).finish()
    }
}

impl GpuCommand {
    pub fn decode(
        cmd: &mut Reader,
    ) -> Result<(virtio_gpu_ctrl_hdr, GpuCommand), GpuCommandDecodeError> {
        use GpuCommand::*;
        let hdr = cmd.read_obj::<virtio_gpu_ctrl_hdr>()?;
        let command = match hdr.type_ {
            VIRTIO_GPU_CMD_GET_DISPLAY_INFO => GetDisplayInfo,
            VIRTIO_GPU_CMD_GET_EDID => GetEdid(cmd.read_obj()?),
            VIRTIO_GPU_CMD_RESOURCE_CREATE_2D => ResourceCreate2d(cmd.read_obj()?),
            VIRTIO_GPU_CMD_RESOURCE_UNREF => ResourceUnref(cmd.read_obj()?),
            VIRTIO_GPU_CMD_SET_SCANOUT => SetScanout(cmd.read_obj()?),
            VIRTIO_GPU_CMD_SET_SCANOUT_BLOB => SetScanoutBlob(cmd.read_obj()?),
            VIRTIO_GPU_CMD_RESOURCE_FLUSH => ResourceFlush(cmd.read_obj()?),
            VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D => TransferToHost2d(cmd.read_obj()?),
            VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING => ResourceAttachBacking(cmd.read_obj()?),
            VIRTIO_GPU_CMD_RESOURCE_DETACH_BACKING => ResourceDetachBacking(cmd.read_obj()?),
            VIRTIO_GPU_CMD_GET_CAPSET_INFO => GetCapsetInfo(cmd.read_obj()?),
            VIRTIO_GPU_CMD_GET_CAPSET => GetCapset(cmd.read_obj()?),
            VIRTIO_GPU_CMD_CTX_CREATE => CtxCreate(cmd.read_obj()?),
            VIRTIO_GPU_CMD_CTX_DESTROY => CtxDestroy(cmd.read_obj()?),
            VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE => CtxAttachResource(cmd.read_obj()?),
            VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE => CtxDetachResource(cmd.read_obj()?),
            VIRTIO_GPU_CMD_RESOURCE_CREATE_3D => ResourceCreate3d(cmd.read_obj()?),
            VIRTIO_GPU_CMD_TRANSFER_TO_HOST_3D => TransferToHost3d(cmd.read_obj()?),
            VIRTIO_GPU_CMD_TRANSFER_FROM_HOST_3D => TransferFromHost3d(cmd.read_obj()?),
            VIRTIO_GPU_CMD_SUBMIT_3D => CmdSubmit3d(cmd.read_obj()?),
            VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB => ResourceCreateBlob(cmd.read_obj()?),
            VIRTIO_GPU_CMD_RESOURCE_MAP_BLOB => ResourceMapBlob(cmd.read_obj()?),
            VIRTIO_GPU_CMD_RESOURCE_UNMAP_BLOB => ResourceUnmapBlob(cmd.read_obj()?),
            VIRTIO_GPU_CMD_UPDATE_CURSOR => UpdateCursor(cmd.read_obj()?),
            VIRTIO_GPU_CMD_MOVE_CURSOR => MoveCursor(cmd.read_obj()?),
            VIRTIO_GPU_CMD_RESOURCE_ASSIGN_UUID => ResourceAssignUuid(cmd.read_obj()?),
            _ => return Err(GpuCommandDecodeError::InvalidType(hdr.type_)),
        };
        Ok((hdr, command))
    }
}

// ---------------------------------------------------------------------------
// GpuResponse enum
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub struct GpuResponsePlaneInfo {
    pub stride: u32,
    pub offset: u32,
}

#[derive(Debug)]
pub enum GpuResponse {
    OkNoData,
    OkDisplayInfo(Vec<(u32, u32, bool)>),
    OkEdid(Box<[u8]>),
    OkCapsetInfo {
        capset_id: u32,
        version: u32,
        size: u32,
    },
    OkCapset(Vec<u8>),
    OkResourcePlaneInfo {
        format_modifier: u64,
        plane_info: Vec<GpuResponsePlaneInfo>,
    },
    OkResourceUuid {
        uuid: [u8; 16],
    },
    OkMapInfo {
        map_info: u32,
    },
    ErrUnspec,
    ErrRutabaga(RutabagaError),
    ErrOutOfMemory,
    ErrInvalidScanoutId,
    ErrInvalidResourceId,
    ErrInvalidContextId,
    ErrInvalidParameter,
}

impl From<RutabagaError> for GpuResponse {
    fn from(e: RutabagaError) -> Self {
        GpuResponse::ErrRutabaga(e)
    }
}

impl fmt::Display for GpuResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GpuResponse::ErrRutabaga(e) => write!(f, "renderer error: {e}"),
            _ => Ok(()),
        }
    }
}

#[derive(Error, Debug)]
pub enum GpuResponseEncodeError {
    #[error("I/O error: {0}")]
    IO(std::io::Error),
    #[error("{0} bytes is too big for an EDID blob")]
    EdidTooBig(usize),
    #[error("{0} is more displays than are valid")]
    TooManyDisplays(usize),
    #[error("{0} is more planes than are valid")]
    TooManyPlanes(usize),
}

impl From<std::io::Error> for GpuResponseEncodeError {
    fn from(e: std::io::Error) -> Self {
        GpuResponseEncodeError::IO(e)
    }
}

pub type VirtioGpuResult = std::result::Result<GpuResponse, GpuResponse>;

impl GpuResponse {
    pub fn get_type(&self) -> u32 {
        match self {
            GpuResponse::OkNoData => VIRTIO_GPU_RESP_OK_NODATA,
            GpuResponse::OkDisplayInfo(_) => VIRTIO_GPU_RESP_OK_DISPLAY_INFO,
            GpuResponse::OkEdid(_) => VIRTIO_GPU_RESP_OK_EDID,
            GpuResponse::OkCapsetInfo { .. } => VIRTIO_GPU_RESP_OK_CAPSET_INFO,
            GpuResponse::OkCapset(_) => VIRTIO_GPU_RESP_OK_CAPSET,
            GpuResponse::OkResourcePlaneInfo { .. } => VIRTIO_GPU_RESP_OK_RESOURCE_PLANE_INFO,
            GpuResponse::OkResourceUuid { .. } => VIRTIO_GPU_RESP_OK_RESOURCE_UUID,
            GpuResponse::OkMapInfo { .. } => VIRTIO_GPU_RESP_OK_MAP_INFO,
            GpuResponse::ErrUnspec => VIRTIO_GPU_RESP_ERR_UNSPEC,
            GpuResponse::ErrRutabaga(_) => VIRTIO_GPU_RESP_ERR_UNSPEC,
            GpuResponse::ErrOutOfMemory => VIRTIO_GPU_RESP_ERR_OUT_OF_MEMORY,
            GpuResponse::ErrInvalidScanoutId => VIRTIO_GPU_RESP_ERR_INVALID_SCANOUT_ID,
            GpuResponse::ErrInvalidResourceId => VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID,
            GpuResponse::ErrInvalidContextId => VIRTIO_GPU_RESP_ERR_INVALID_CONTEXT_ID,
            GpuResponse::ErrInvalidParameter => VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER,
        }
    }

    pub fn encode(
        &self,
        flags: u32,
        fence_id: u64,
        ctx_id: u32,
        ring_idx: u8,
        resp: &mut Writer,
    ) -> Result<u32, GpuResponseEncodeError> {
        let hdr = virtio_gpu_ctrl_hdr {
            type_: self.get_type(),
            flags,
            fence_id,
            ctx_id,
            ring_idx,
            padding: Default::default(),
        };
        let len = match self {
            GpuResponse::OkDisplayInfo(info) => {
                if info.len() > VIRTIO_GPU_MAX_SCANOUTS as usize {
                    return Err(GpuResponseEncodeError::TooManyDisplays(info.len()));
                }
                let mut disp_info = virtio_gpu_resp_display_info {
                    hdr,
                    pmodes: Default::default(),
                };
                for (disp_mode, &(width, height, enabled)) in
                    disp_info.pmodes.iter_mut().zip(info.iter())
                {
                    disp_mode.r.width = width;
                    disp_mode.r.height = height;
                    disp_mode.enabled = enabled as u32;
                }
                resp.write_obj(disp_info)?;
                size_of_val(&disp_info)
            }
            GpuResponse::OkEdid(blob) => {
                if blob.len() > EDID_BLOB_MAX_SIZE {
                    return Err(GpuResponseEncodeError::EdidTooBig(blob.len()));
                }
                let mut edid_info = virtio_gpu_resp_edid {
                    hdr,
                    size: blob.len() as u32,
                    edid: [0; EDID_BLOB_MAX_SIZE],
                    padding: Default::default(),
                };
                edid_info.edid[..blob.len()].copy_from_slice(blob);
                resp.write_obj(edid_info)?;
                size_of_val(&edid_info)
            }
            GpuResponse::OkCapsetInfo {
                capset_id,
                version,
                size,
            } => {
                resp.write_obj(virtio_gpu_resp_capset_info {
                    hdr,
                    capset_id: *capset_id,
                    capset_max_version: *version,
                    capset_max_size: *size,
                    padding: 0,
                })?;
                size_of::<virtio_gpu_resp_capset_info>()
            }
            GpuResponse::OkCapset(data) => {
                resp.write_obj(hdr)?;
                resp.write_all(data)?;
                size_of_val(&hdr) + data.len()
            }
            GpuResponse::OkResourcePlaneInfo {
                format_modifier,
                plane_info,
            } => {
                if plane_info.len() > PLANE_INFO_MAX_COUNT {
                    return Err(GpuResponseEncodeError::TooManyPlanes(plane_info.len()));
                }
                let mut strides = [0u32; PLANE_INFO_MAX_COUNT];
                let mut offsets = [0u32; PLANE_INFO_MAX_COUNT];
                for (i, plane) in plane_info.iter().enumerate() {
                    strides[i] = plane.stride;
                    offsets[i] = plane.offset;
                }
                let plane_resp = virtio_gpu_resp_resource_plane_info {
                    hdr,
                    count: plane_info.len() as u32,
                    padding: 0,
                    format_modifier: *format_modifier,
                    strides,
                    offsets,
                };
                if resp.available_bytes() >= size_of_val(&plane_resp) {
                    resp.write_obj(plane_resp)?;
                    size_of_val(&plane_resp)
                } else {
                    resp.write_obj(virtio_gpu_ctrl_hdr {
                        type_: VIRTIO_GPU_RESP_OK_NODATA,
                        ..hdr
                    })?;
                    size_of_val(&hdr)
                }
            }
            GpuResponse::OkResourceUuid { uuid } => {
                let r = virtio_gpu_resp_resource_uuid { hdr, uuid: *uuid };
                resp.write_obj(r)?;
                size_of_val(&r)
            }
            GpuResponse::OkMapInfo { map_info } => {
                let r = virtio_gpu_resp_map_info {
                    hdr,
                    map_info: *map_info,
                    padding: Default::default(),
                };
                resp.write_obj(r)?;
                size_of_val(&r)
            }
            _ => {
                resp.write_obj(hdr)?;
                size_of_val(&hdr)
            }
        };
        Ok(len as u32)
    }
}
