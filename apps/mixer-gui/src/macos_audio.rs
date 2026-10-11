//! 直接调用 CoreAudio —— 只为「切回真实麦克风」这一件事。
//!
//! 为什么不用 crate：需要的只有两个 C 函数（`AudioObjectGetPropertyData` /
//! `AudioObjectSetPropertyData`）+ 一个结构体，自己声明比引入依赖更省事，
//! 也避免和 cpal 底下的 coreaudio-rs 版本打架。
//!
//! ⚠️ 实测注意：**返回 0（成功）不代表真的改了**。豆包那个虚拟声卡驱动就是
//! 返回成功但默认设备纹丝不动。所以这里所有 set 之后都要 `get` 回读确认。

#![cfg(target_os = "macos")]

use std::ffi::CStr;

type OSStatus = i32;
type AudioObjectID = u32;

const K_AUDIO_OBJECT_SYSTEM_OBJECT: AudioObjectID = 1;
const SCOPE_GLOBAL: u32 = u32::from_be_bytes(*b"glob"); // 'glob'
const SCOPE_INPUT: u32 = u32::from_be_bytes(*b"inpt"); // 'inpt'
const ELEMENT_MAIN: u32 = 0;

fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioObjectPropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

impl AudioObjectPropertyAddress {
    const fn new(selector: u32, scope: u32) -> Self {
        Self {
            selector,
            scope,
            element: ELEMENT_MAIN,
        }
    }
}

#[link(name = "CoreAudio", kind = "framework")]
extern "C" {
    fn AudioObjectGetPropertyData(
        object: AudioObjectID,
        addr: *const AudioObjectPropertyAddress,
        qualifier_size: u32,
        qualifier: *const std::ffi::c_void,
        data_size: *mut u32,
        data: *mut std::ffi::c_void,
    ) -> OSStatus;

    fn AudioObjectSetPropertyData(
        object: AudioObjectID,
        addr: *const AudioObjectPropertyAddress,
        qualifier_size: u32,
        qualifier: *const std::ffi::c_void,
        data_size: u32,
        data: *const std::ffi::c_void,
    ) -> OSStatus;

    fn AudioObjectGetPropertyDataSize(
        object: AudioObjectID,
        addr: *const AudioObjectPropertyAddress,
        qualifier_size: u32,
        qualifier: *const std::ffi::c_void,
        data_size: *mut u32,
    ) -> OSStatus;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFStringGetCString(
        s: *const std::ffi::c_void,
        buf: *mut i8,
        size: isize,
        encoding: u32,
    ) -> bool;
}

/// 取一个 u32 属性（设备 id 都用它）。
fn get_u32(object: AudioObjectID, selector: &[u8; 4], scope: u32) -> Option<u32> {
    let addr = AudioObjectPropertyAddress::new(fourcc(selector), scope);
    let mut val: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let rc = unsafe {
        AudioObjectGetPropertyData(
            object,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
            &mut val as *mut u32 as *mut std::ffi::c_void,
        )
    };
    (rc == 0).then_some(val)
}

/// 设备名（`lnam`）。
fn device_name(id: AudioObjectID) -> String {
    let addr = AudioObjectPropertyAddress::new(fourcc(b"lnam"), SCOPE_GLOBAL);
    let mut cfstr: *const std::ffi::c_void = std::ptr::null();
    let mut size = std::mem::size_of::<*const std::ffi::c_void>() as u32;
    let rc = unsafe {
        AudioObjectGetPropertyData(
            id,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
            &mut cfstr as *mut _ as *mut std::ffi::c_void,
        )
    };
    if rc != 0 || cfstr.is_null() {
        return String::new();
    }
    let mut buf = [0i8; 256];
    // kCFStringEncodingUTF8 = 0x08000100
    let ok = unsafe { CFStringGetCString(cfstr, buf.as_mut_ptr(), buf.len() as isize, 0x0800_0100) };
    if !ok {
        return String::new();
    }
    unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

/// 系统当前默认输入设备。
pub fn default_input() -> Option<(u32, String)> {
    let id = get_u32(
        K_AUDIO_OBJECT_SYSTEM_OBJECT,
        b"dIn ",
        SCOPE_GLOBAL,
    )?;
    Some((id, device_name(id)))
}

/// 所有**有输入流**的设备（麦克风才算）。
pub fn input_devices() -> Vec<(u32, String)> {
    let addr = AudioObjectPropertyAddress::new(fourcc(b"dev#"), SCOPE_GLOBAL);
    let mut size: u32 = 0;
    let rc = unsafe {
        AudioObjectGetPropertyDataSize(
            K_AUDIO_OBJECT_SYSTEM_OBJECT,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
        )
    };
    if rc != 0 {
        return Vec::new();
    }
    let n = size as usize / std::mem::size_of::<u32>();
    let mut ids = vec![0u32; n];
    let rc = unsafe {
        AudioObjectGetPropertyData(
            K_AUDIO_OBJECT_SYSTEM_OBJECT,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
            ids.as_mut_ptr() as *mut std::ffi::c_void,
        )
    };
    if rc != 0 {
        return Vec::new();
    }
    ids.into_iter()
        .filter(|&id| has_input_stream(id))
        .map(|id| (id, device_name(id)))
        .collect()
}

/// 设备有没有输入流（用它区分麦克风和扬声器）。
pub fn has_input_stream(id: AudioObjectID) -> bool {
    let addr = AudioObjectPropertyAddress::new(fourcc(b"stm#"), SCOPE_INPUT);
    let mut size: u32 = 0;
    let rc = unsafe {
        AudioObjectGetPropertyDataSize(id, &addr, 0, std::ptr::null(), &mut size)
    };
    rc == 0 && size > 0
}

/// 所有**有输出流**的设备。
///
/// ⚠️ 热插拔检测必须用它，**不能用 cpal 的 `devices()`** ——
/// 实测：在另一个线程里创建第二个 cpal 后端做枚举，会和主线程"开音频流"直接撞死
/// （`start()` 卡住不动，日志停在参数打印那一行之后）。
/// 这里的实现只做一次轻量的 CoreAudio 查询，不碰 cpal。
pub fn output_devices() -> Vec<(u32, String)> {
    let addr = AudioObjectPropertyAddress::new(fourcc(b"dev#"), SCOPE_GLOBAL);
    let mut size: u32 = 0;
    let rc = unsafe {
        AudioObjectGetPropertyDataSize(
            K_AUDIO_OBJECT_SYSTEM_OBJECT,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
        )
    };
    if rc != 0 {
        return Vec::new();
    }
    let n = size as usize / std::mem::size_of::<u32>();
    let mut ids = vec![0u32; n];
    let rc = unsafe {
        AudioObjectGetPropertyData(
            K_AUDIO_OBJECT_SYSTEM_OBJECT,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
            ids.as_mut_ptr() as *mut std::ffi::c_void,
        )
    };
    if rc != 0 {
        return Vec::new();
    }
    ids.into_iter()
        .filter(|&id| has_output_stream(id))
        .map(|id| (id, device_name(id)))
        .collect()
}

/// 设备有没有输出流。
pub fn has_output_stream(id: AudioObjectID) -> bool {
    let addr = AudioObjectPropertyAddress::new(
        fourcc(b"stm#"),
        u32::from_be_bytes(*b"outp"),
    );
    let mut size: u32 = 0;
    let rc = unsafe {
        AudioObjectGetPropertyDataSize(id, &addr, 0, std::ptr::null(), &mut size)
    };
    rc == 0 && size > 0
}

/// 真实（非虚拟）输入设备里最像"麦克风"的一个。
///
/// 优先内置麦克风，其次任意非虚拟设备 —— 用来在没有记录时兜底。
pub fn first_real_input() -> Option<(u32, String)> {
    let all = input_devices();
    let real: Vec<_> = all
        .iter()
        .filter(|(_, n)| !audio_core::device::is_virtual_name(n))
        .collect();
    real.iter()
        .find(|(_, n)| n.contains("MacBook") || n.contains("Built-in") || n.contains("内建"))
        .or_else(|| real.first())
        .map(|(id, n)| (*id, n.clone()))
}

/// 把系统默认输入切到 `id`。
///
/// **返回是否真的切成功了**（回读确认）—— 因为某些驱动会"返回成功但不生效"。
pub fn set_default_input(id: AudioObjectID) -> bool {
    let addr = AudioObjectPropertyAddress::new(fourcc(b"dIn "), SCOPE_GLOBAL);
    let val = id;
    let rc = unsafe {
        AudioObjectSetPropertyData(
            K_AUDIO_OBJECT_SYSTEM_OBJECT,
            &addr,
            0,
            std::ptr::null(),
            std::mem::size_of::<u32>() as u32,
            &val as *const u32 as *const std::ffi::c_void,
        )
    };
    if rc != 0 {
        return false;
    }
    // ⚠️ 必须回读：豆包虚拟声卡就是返回 0 但根本没变
    std::thread::sleep(std::time::Duration::from_millis(250));
    get_u32(K_AUDIO_OBJECT_SYSTEM_OBJECT, b"dIn ", SCOPE_GLOBAL) == Some(id)
}

/// 按名字找输入设备 id（名字完全一致）。
pub fn find_input_by_name(name: &str) -> Option<(u32, String)> {
    input_devices().into_iter().find(|(_, n)| n == name)
}
