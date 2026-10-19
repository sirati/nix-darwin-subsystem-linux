//! Minimal CoreAudio FFI: names of the default output and input devices.

use std::ffi::{c_char, c_void, CStr};
use std::ptr;

type AudioObjectID = u32;
type OSStatus = i32;
type CFStringRef = *const c_void;
type CFIndex = isize;

#[repr(C)]
struct AudioObjectPropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

const K_AUDIO_OBJECT_SYSTEM_OBJECT: AudioObjectID = 1;
const K_AUDIO_OBJECT_UNKNOWN: AudioObjectID = 0;
const K_DEFAULT_OUTPUT_DEVICE: u32 = fourcc(b"dOut");
const K_DEFAULT_INPUT_DEVICE: u32 = fourcc(b"dIn ");
const K_OBJECT_PROPERTY_NAME: u32 = fourcc(b"lnam");
const K_SCOPE_GLOBAL: u32 = fourcc(b"glob");
const K_ELEMENT_MAIN: u32 = 0;
const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

#[link(name = "CoreAudio", kind = "framework")]
extern "C" {
    fn AudioObjectGetPropertyData(
        object: AudioObjectID,
        address: *const AudioObjectPropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        data_size: *mut u32,
        data: *mut c_void,
    ) -> OSStatus;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFStringGetLength(s: CFStringRef) -> CFIndex;
    fn CFStringGetMaximumSizeForEncoding(len: CFIndex, encoding: u32) -> CFIndex;
    fn CFStringGetCString(s: CFStringRef, buf: *mut c_char, size: CFIndex, encoding: u32) -> u8;
    fn CFRelease(obj: *const c_void);
}

unsafe fn get<T>(object: AudioObjectID, selector: u32, out: &mut T) -> bool {
    let addr = AudioObjectPropertyAddress { selector, scope: K_SCOPE_GLOBAL, element: K_ELEMENT_MAIN };
    let mut size = std::mem::size_of::<T>() as u32;
    let status = AudioObjectGetPropertyData(object, &addr, 0, ptr::null(), &mut size, out as *mut T as *mut c_void);
    status == 0 && size as usize == std::mem::size_of::<T>()
}

unsafe fn cfstring_to_string(s: CFStringRef) -> Option<String> {
    let max = CFStringGetMaximumSizeForEncoding(CFStringGetLength(s), K_CF_STRING_ENCODING_UTF8) + 1;
    let mut buf = vec![0 as c_char; usize::try_from(max).ok()?];
    if CFStringGetCString(s, buf.as_mut_ptr(), max, K_CF_STRING_ENCODING_UTF8) == 0 {
        return None;
    }
    Some(CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned())
}

fn default_device_name(selector: u32) -> Option<String> {
    unsafe {
        let mut dev: AudioObjectID = K_AUDIO_OBJECT_UNKNOWN;
        if !get(K_AUDIO_OBJECT_SYSTEM_OBJECT, selector, &mut dev) || dev == K_AUDIO_OBJECT_UNKNOWN {
            return None;
        }
        let mut name: CFStringRef = ptr::null();
        if !get(dev, K_OBJECT_PROPERTY_NAME, &mut name) || name.is_null() {
            return None;
        }
        // The property returns a +1 reference.
        let out = cfstring_to_string(name);
        CFRelease(name);
        out
    }
}

/// (default output name, default input name)
pub fn defaults() -> (Option<String>, Option<String>) {
    (default_device_name(K_DEFAULT_OUTPUT_DEVICE), default_device_name(K_DEFAULT_INPUT_DEVICE))
}
