#![allow(static_mut_refs)]

use crate::{
    cpp_string::{ResourceLocation, StackString},
    jniopts::OPTS,
};
use cxx::CxxString;
use libc::{c_char, c_int, c_void, off64_t, off_t, size_t};
use materialbin::{
    bgfx_shader::BgfxShader, pass::ShaderStage, CompiledMaterialDefinition, MinecraftVersion,
};
use memchr::memmem::Finder;
use ndk::asset::{Asset, AssetManager};
use ndk_sys::{AAsset, AAssetManager};
use once_cell::sync::Lazy;
use scroll::Pread;
use std::{
    cell::UnsafeCell,
    collections::HashMap,
    ffi::{CStr, OsStr},
    io::{self, Cursor, Read, Seek, Write},
    ops::{Deref, DerefMut},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        LazyLock, Mutex, OnceLock,
    },
};
static MC_FILELOADER: LazyLock<Mutex<FileLoader>> =
    LazyLock::new(|| Mutex::new(FileLoader { last_buffer: None }));
#[derive(PartialEq, Eq, Hash)]
struct AAssetPtr(*const ndk_sys::AAsset);
unsafe impl Send for AAssetPtr {}

static MC_VERSION: OnceLock<Option<MinecraftVersion>> = OnceLock::new();
static IS_1_21_100: AtomicBool = AtomicBool::new(false);
static mut WANTED_ASSETS: Lazy<UnsafeCell<HashMap<AAssetPtr, Buffer>>> =
    Lazy::new(|| UnsafeCell::new(HashMap::new()));

fn get_current_mcver(man: ndk::asset::AssetManager) -> Option<MinecraftVersion> {
    let mut file = get_uitext(man)?;
    let mut buf = Vec::with_capacity(file.length());
    if file.read_to_end(&mut buf).is_err() {
        return None;
    };

    for version in materialbin::ALL_VERSIONS.into_iter().rev() {
        if let Ok(_shader) = buf.pread_with::<CompiledMaterialDefinition>(0, version) {
            if memchr::memmem::find(&buf, b"v_dithering").is_some() {
                IS_1_21_100.store(true, Ordering::Release);
            }
            return Some(version);
        };
    }
    None
}

fn get_uitext(man: ndk::asset::AssetManager) -> Option<Asset> {
    const NEW: &CStr = c"assets/renderer/materials/RenderChunk.material.bin";
    const OLD: &CStr = c"renderer/materials/RenderChunk.material.bin";
    for path in [NEW, OLD] {
        if let Some(asset) = man.open(path) {
            return Some(asset);
        }
    }
    None
}
macro_rules! folder_list {
    ($( apk: $apk_folder:literal -> pack: $pack_folder:expr),
        *,
    ) => {
        [
            $(($apk_folder, $pack_folder)),*,
        ]
    }
}
pub unsafe extern "C" fn open(
    man: *mut AAssetManager,
    fname: *const c_char,
    mode: c_int,
) -> *mut AAsset {
    let aasset = unsafe { ndk_sys::AAssetManager_open(man, fname, mode) };
    let pointer = match std::ptr::NonNull::new(man) {
        Some(yay) => yay,
        None => return aasset,
    };
    let manager = unsafe { ndk::asset::AssetManager::from_ptr(pointer) };
    let c_str = unsafe { CStr::from_ptr(fname) };
    let raw_cstr = c_str.to_bytes();
    let os_str = OsStr::from_bytes(raw_cstr);
    let c_path: &Path = Path::new(os_str);
    let mut sus = MC_FILELOADER.lock().unwrap();
    if let Some(yay) = sus.get_file(c_path, manager) {
        WANTED_ASSETS.get_mut().insert(AAssetPtr(aasset), yay);
    }
    return aasset;
}
macro_rules! handle_result {
    ($expr:expr) => {
        match $expr {
            Ok(val) => val,
            Err(_e) => {
                return -1;
            }
        }
    };
}
#[allow(clippy::unused_io_amount)]
fn opt_path_join(mut bytes: Pin<&mut CxxString>, paths: &[&Path]) {
    let total_len: usize = paths.iter().map(|p| p.as_os_str().len()).sum();
    bytes.as_mut().reserve(total_len);
    let mut writer = bytes;
    for path in paths {
        let osstr = path.as_os_str().as_bytes();
        writer
            .write(osstr)
            .expect("Error while writing path to stack path");
    }
}
fn process_material(man: AssetManager, data: &[u8]) -> Option<Vec<u8>> {
    let mcver = MC_VERSION.get_or_init(|| get_current_mcver(man));
    let mcver = (*mcver)?;
    let opts = OPTS.lock().unwrap();
    for version in opts.autofixer_versions.iter() {
        let version = *version;
        let mut material: CompiledMaterialDefinition = match data.pread_with(0, version) {
            Ok(data) => data,
            Err(_e) => {
                continue;
            }
        };
        let needs_lightmap_fix = IS_1_21_100.load(Ordering::Acquire)
            && version != MinecraftVersion::V1_21_110
            && (material.name == "RenderChunk" || material.name == "RenderChunkPrepass")
            && opts.handle_lightmaps;
        let needs_sampler_fix = material.name == "RenderChunk"
            && mcver >= MinecraftVersion::V1_20_80
            && version <= MinecraftVersion::V1_19_60
            && opts.handle_texturelods;
        if version == mcver && !needs_lightmap_fix && !needs_sampler_fix {
            return None;
        }
        if needs_lightmap_fix {
            handle_lightmaps(&mut material);
        }
        if needs_sampler_fix {
            handle_samplers(&mut material);
        }
        let mut output = Vec::with_capacity(data.len());
        if material.write(&mut output, mcver).is_err() {
            return None;
        }
        return Some(output);
    }

    None
}
fn handle_lightmaps(materialbin: &mut CompiledMaterialDefinition) {
    let finder = Finder::new(b"void main");
    let replace_with = b"
#define a_texcoord1 vec2(fract(a_texcoord1.x*15.9375)+0.0001,floor(a_texcoord1.x*15.9375)*0.0625+0.0001)
void main";
    for (_, pass) in &mut materialbin.passes {
        for variants in &mut pass.variants {
            for (stage, code) in &mut variants.shader_codes {
                if stage.stage == ShaderStage::Vertex {
                    let blob = &mut code.bgfx_shader_data;
                    let Ok(mut bgfx) = blob.pread::<BgfxShader>(0) else {
                        continue;
                    };
                    replace_bytes(&mut bgfx.code, &finder, b"void main", replace_with);
                    blob.clear();
                    let _unused = bgfx.write(blob);
                }
            }
        }
    }
}
fn handle_samplers(materialbin: &mut CompiledMaterialDefinition) {
    let pattern = b"void main ()";
    let replace_with = b"
#if __VERSION__ >= 300
 #define texture(tex,uv) textureLod(tex,uv,0.0)
#else
 #define texture2D(tex,uv) texture2DLod(tex,uv,0.0)
#endif
void main ()";
    let finder = Finder::new(pattern);
    for (_passes, pass) in &mut materialbin.passes {
        if _passes == "AlphaTest" || _passes == "Opaque" {
            for variants in &mut pass.variants {
                for (stage, code) in &mut variants.shader_codes {
                    if stage.stage == ShaderStage::Fragment && stage.platform_name == "ESSL_100" {
                        let mut bgfx: BgfxShader = code.bgfx_shader_data.pread(0).unwrap();
                        replace_bytes(&mut bgfx.code, &finder, pattern, replace_with);
                        code.bgfx_shader_data.clear();
                        bgfx.write(&mut code.bgfx_shader_data).unwrap();
                    }
                }
            }
        }
    }
}

fn replace_bytes(codebuf: &mut Vec<u8>, finder: &Finder, pattern: &[u8], replace_with: &[u8]) {
    let sus = match finder.find(codebuf) {
        Some(yay) => yay,
        None => return,
    };
    codebuf.splice(sus..sus + pattern.len(), replace_with.iter().cloned());
}
pub unsafe extern "C" fn seek64(aasset: *mut AAsset, off: off64_t, whence: c_int) -> off64_t {
    let file = match WANTED_ASSETS.get_mut().get_mut(&AAssetPtr(aasset)) {
        Some(file) => file,
        None => return ndk_sys::AAsset_seek64(aasset, off, whence),
    };
    handle_result!(seek_facade(off, whence, file).try_into())
}

pub unsafe extern "C" fn seek(aasset: *mut AAsset, off: off_t, whence: c_int) -> off_t {
    let wanted_assets = WANTED_ASSETS.get_mut();
    let file = match wanted_assets.get_mut(&AAssetPtr(aasset)) {
        Some(file) => file,
        None => return ndk_sys::AAsset_seek(aasset, off, whence),
    };
    handle_result!(seek_facade(off.into(), whence, file).try_into())
}

pub unsafe extern "C" fn read(aasset: *mut AAsset, buf: *mut c_void, count: size_t) -> c_int {
    let wanted_assets = WANTED_ASSETS.get_mut();
    let file = match wanted_assets.get_mut(&AAssetPtr(aasset)) {
        Some(file) => file,
        None => return ndk_sys::AAsset_read(aasset, buf, count),
    };
    let rs_buffer = core::slice::from_raw_parts_mut(buf as *mut u8, count);
    let read_total = match (*file).read(rs_buffer) {
        Ok(n) => n,
        Err(_e) => {
            return -1 as c_int;
        }
    };
    handle_result!(read_total.try_into())
}

pub unsafe extern "C" fn len(aasset: *mut AAsset) -> off_t {
    let wanted_assets = WANTED_ASSETS.get_mut();
    let file = match wanted_assets.get(&AAssetPtr(aasset)) {
        Some(file) => file,
        None => return ndk_sys::AAsset_getLength(aasset),
    };
    handle_result!(file.get_ref().len().try_into())
}

pub unsafe extern "C" fn len64(aasset: *mut AAsset) -> off64_t {
    let wanted_assets = WANTED_ASSETS.get_mut();
    let file = match wanted_assets.get(&AAssetPtr(aasset)) {
        Some(file) => file,
        None => return ndk_sys::AAsset_getLength64(aasset),
    };
    handle_result!(file.get_ref().len().try_into())
}

pub unsafe extern "C" fn rem(aasset: *mut AAsset) -> off_t {
    let wanted_assets = WANTED_ASSETS.get_mut();
    let file = match wanted_assets.get(&AAssetPtr(aasset)) {
        Some(file) => file,
        None => return ndk_sys::AAsset_getRemainingLength(aasset),
    };
    handle_result!((file.get_ref().len() - file.position() as usize).try_into())
}

pub unsafe extern "C" fn rem64(aasset: *mut AAsset) -> off64_t {
    let wanted_assets = WANTED_ASSETS.get_mut();
    let file = match wanted_assets.get(&AAssetPtr(aasset)) {
        Some(file) => file,
        None => return ndk_sys::AAsset_getRemainingLength64(aasset),
    };
    handle_result!((file.get_ref().len() - file.position() as usize).try_into())
}

pub unsafe extern "C" fn close(aasset: *mut AAsset) {
    let wanted_assets = WANTED_ASSETS.get_mut();
    if let Some(buffer) = wanted_assets.remove(&AAssetPtr(aasset)) {
        MC_FILELOADER.lock().unwrap().last_buffer = Some(buffer);
    }
    ndk_sys::AAsset_close(aasset);
}

pub unsafe extern "C" fn get_buffer(aasset: *mut AAsset) -> *const c_void {
    let wanted_assets = WANTED_ASSETS.get_mut();
    let file = match wanted_assets.get_mut(&AAssetPtr(aasset)) {
        Some(file) => file,
        None => return ndk_sys::AAsset_getBuffer(aasset),
    };
    file.get_ref().as_ptr().cast()
}

pub unsafe extern "C" fn fd_dummy(
    aasset: *mut AAsset,
    out_start: *mut off_t,
    out_len: *mut off_t,
) -> c_int {
    let wanted_assets = WANTED_ASSETS.get_mut();
    match wanted_assets.get(&AAssetPtr(aasset)) {
        Some(_) => -1,
        None => ndk_sys::AAsset_openFileDescriptor(aasset, out_start, out_len),
    }
}

pub unsafe extern "C" fn fd_dummy64(
    aasset: *mut AAsset,
    out_start: *mut off64_t,
    out_len: *mut off64_t,
) -> c_int {
    let wanted_assets = WANTED_ASSETS.get_mut();
    match wanted_assets.get(&AAssetPtr(aasset)) {
        Some(_) => -1,
        None => ndk_sys::AAsset_openFileDescriptor64(aasset, out_start, out_len),
    }
}

pub unsafe extern "C" fn is_alloc(aasset: *mut AAsset) -> c_int {
    let wanted_assets = WANTED_ASSETS.get_mut();
    match wanted_assets.get(&AAssetPtr(aasset)) {
        Some(_) => false as c_int,
        None => ndk_sys::AAsset_isAllocated(aasset),
    }
}

fn seek_facade(offset: i64, whence: c_int, file: &mut Buffer) -> i64 {
    let offset = match whence {
        libc::SEEK_SET => {
            let u64_off = match u64::try_from(offset) {
                Ok(uoff) => uoff,
                Err(_e) => {
                    return -1;
                }
            };
            io::SeekFrom::Start(u64_off)
        }
        libc::SEEK_CUR => io::SeekFrom::Current(offset),
        libc::SEEK_END => io::SeekFrom::End(offset),
        _ => {
            return -1;
        }
    };
    match file.seek(offset) {
        Ok(new_offset) => match new_offset.try_into() {
            Ok(int) => int,
            Err(_err) => {
                -1
            }
        },
        Err(_err) => {
            -1
        }
    }
}

enum BufferCursor {
    Vec(Cursor<Vec<u8>>),
    Cxx(Cursor<StackString>),
}
impl Read for BufferCursor {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Vec(v) => v.read(buf),
            Self::Cxx(cxx) => cxx.read(buf),
        }
    }
}
impl Seek for BufferCursor {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        match self {
            Self::Vec(v) => v.seek(pos),
            Self::Cxx(cxx) => cxx.seek(pos),
        }
    }
}
impl BufferCursor {
    fn position(&self) -> u64 {
        match self {
            Self::Vec(v) => v.position(),
            Self::Cxx(cxx) => cxx.position(),
        }
    }
    fn get_ref(&self) -> &[u8] {
        match self {
            Self::Vec(v) => v.get_ref(),
            Self::Cxx(cxx) => cxx.get_ref().as_ref(),
        }
    }
}

struct FileLoader {
    last_buffer: Option<Buffer>,
}
impl FileLoader {
    fn get_file(&mut self, path: &Path, manager: AssetManager) -> Option<Buffer> {
        let stripped = path.strip_prefix("assets/").unwrap_or(path);
        
        if stripped.as_os_str().as_encoded_bytes().ends_with(b".material.bin") {
            self.last_buffer = None; 
        } else if let Some(mut cache) = self.last_buffer.take_if(|c| c.name == path) {
            cache.rewind().unwrap();
            return Some(cache);
        }
        let replacement_list = folder_list! {
            apk: "gui/dist/hbui/" -> pack: "hbui/",
            apk: "skin_packs/persona/" -> pack: "persona/",
            apk: "renderer/" -> pack: "renderer/",
            apk: "resource_packs/vanilla/cameras/" -> pack: "vanilla_cameras/",
        };
        for replacement in replacement_list {
            if let Ok(file) = stripped.strip_prefix(replacement.0) {
                let mut cxx_storage = StackString::new();
                let mut cxx_ptr = unsafe { cxx_storage.init("") };
                let Some(loadfn) = crate::RPM_LOAD.get() else {
                    return None;
                };
                let mut resource_loc = ResourceLocation::new();
                let mut cpppath = ResourceLocation::get_path(&mut resource_loc);
                opt_path_join(cpppath.as_mut(), &[Path::new(replacement.1), file]);
                let packm_ptr = crate::PACKM_OBJ.load(Ordering::Acquire);
                if packm_ptr.is_null() {
                    return None;
                }
                unsafe {
                    loadfn(packm_ptr, resource_loc, cxx_ptr.as_mut());
                }
                if cxx_ptr.is_empty() {
                    return None;
                }
                let buffer = if file
                    .as_os_str()
                    .as_encoded_bytes()
                    .ends_with(b".material.bin")
                {
                    match process_material(manager, cxx_ptr.as_bytes()) {
                        Some(updated) => BufferCursor::Vec(Cursor::new(updated)),
                        None => BufferCursor::Cxx(Cursor::new(cxx_storage)),
                    }
                } else {
                    BufferCursor::Cxx(Cursor::new(cxx_storage))
                };
                let cache = Buffer::new(path.to_path_buf(), buffer);
                return Some(cache);
            }
        }
        None
    }
}
struct Buffer {
    name: PathBuf,
    object: BufferCursor,
}
impl Buffer {
    fn new(name: PathBuf, object: BufferCursor) -> Self {
        Self { name, object }
    }
}
impl Deref for Buffer {
    type Target = BufferCursor;
    fn deref(&self) -> &Self::Target {
        &self.object
    }
}
impl DerefMut for Buffer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.object
    }
}