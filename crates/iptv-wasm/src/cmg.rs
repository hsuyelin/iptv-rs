use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use wasmtime::{Caller, Linker, Memory, Ref, Store, Table};

use crate::{
    assets::CmgImage,
    error::{Context, Result},
};

const INITIAL_PAGES: u32 = 256;
const MAX_PAGES: u32 = 1536;
const TABLE_MIN: u32 = 560;
const DYNAMIC_BASE: i32 = 5_260_816;
const EMT_STACK_SIZE: i32 = 1_048_576;
const EB_SIZE: i32 = 378_592;
const DYNAMIC_TOP_AFTER_RUNTIME_ALLOCS: i32 = DYNAMIC_BASE + EMT_STACK_SIZE + EB_SIZE;
const DYNAMICTOP_PTR: i32 = 17_904;
const TEMP_DOUBLE_PTR: i32 = 17_920;
const EMT_STACK_TOP: i32 = DYNAMIC_BASE;
const EB: i32 = DYNAMIC_BASE + EMT_STACK_SIZE;
const MEMORY_EXTEND: usize = 2048;

struct CmgState {
    memory: Option<Memory>,
    table: Option<Table>,
    malloc: Option<wasmtime::TypedFunc<i32, i32>>,
    free: Option<wasmtime::TypedFunc<i32, ()>>,
    pending_fetches: Vec<PendingFetch>,
    next_fetch_handle: i32,
    temp_ret0: i32,
    function_pointers: Vec<Option<u32>>,
    emval: EmvalState,
    location: CmgLocation,
}

#[derive(Debug)]
struct PendingFetch {
    ptr: i32,
    url: String,
    onsuccess: i32,
    onerror: i32,
}

#[derive(Clone, Debug)]
enum EmvalValue {
    Undefined,
    Null,
    Bool(bool),
    Location,
    String(String),
    Destructors(Vec<i32>),
}

#[derive(Debug)]
struct EmvalState {
    handles: Vec<Option<EmvalValue>>,
    free: Vec<i32>,
    std_string_type: Option<i32>,
    location_handle: Option<i32>,
}

impl Default for EmvalState {
    fn default() -> Self {
        Self {
            handles: vec![
                None,
                Some(EmvalValue::Undefined),
                Some(EmvalValue::Null),
                Some(EmvalValue::Bool(true)),
                Some(EmvalValue::Bool(false)),
            ],
            free: Vec::new(),
            std_string_type: None,
            location_handle: None,
        }
    }
}

#[derive(Clone, Debug)]
struct CmgLocation {
    href: String,
    host: String,
    hostname: String,
    origin: String,
    protocol: String,
}

impl CmgLocation {
    fn from_href(href: &str) -> Self {
        let parsed = url::Url::parse(href).ok();
        let protocol = parsed
            .as_ref()
            .map(|url| format!("{}:", url.scheme()))
            .unwrap_or_else(|| "https:".to_string());
        let hostname = parsed
            .as_ref()
            .and_then(|url| url.host_str())
            .unwrap_or("www.yangshipin.cn")
            .to_string();
        let host = parsed
            .as_ref()
            .and_then(|url| {
                url.host_str().map(|host| match url.port() {
                    Some(port) => format!("{host}:{port}"),
                    None => host.to_string(),
                })
            })
            .unwrap_or_else(|| hostname.clone());
        let origin = parsed
            .as_ref()
            .map(|url| {
                let mut value = format!("{}://{}", url.scheme(), hostname);
                if let Some(port) = url.port() {
                    value.push_str(&format!(":{port}"));
                }
                value
            })
            .unwrap_or_else(|| "https://www.yangshipin.cn".to_string());
        Self {
            href: href.to_string(),
            host,
            hostname,
            origin,
            protocol,
        }
    }
}

impl Default for CmgLocation {
    fn default() -> Self {
        Self::from_href("https://www.yangshipin.cn/tv/home?pid=600099502")
    }
}

/// One CMG WASM instance with its own memory and state.
pub struct CmgRuntime {
    image: Arc<CmgImage>,
    store: Store<CmgState>,
    memory: Memory,
    init_player: wasmtime::TypedFunc<i32, i32>,
    update_player: wasmtime::TypedFunc<i32, i32>,
    dec_live: Vec<wasmtime::TypedFunc<(i32, i32, i32, i32), i32>>,
    js_malloc: wasmtime::TypedFunc<i32, i32>,
    js_free: wasmtime::TypedFunc<i32, ()>,
    dyn_call_vi: wasmtime::TypedFunc<(i32, i32), ()>,
    vmp_tag: String,
    update_calls: usize,
    live_export_calls: usize,
    module_dec_calls: usize,
}

impl CmgRuntime {
    /// Instantiates the shared image for a page URL (it is exposed to the script as
    /// `window.location`).
    ///
    /// # Errors
    /// Returns [`crate::WasmError`] when instantiation or initialization fails.
    pub fn new(image: &Arc<CmgImage>, page_url: &str) -> Result<Self> {
        let module = image.module();
        let engine = module.engine();
        let mut store = Store::new(
            engine,
            CmgState {
                memory: None,
                table: None,
                malloc: None,
                free: None,
                pending_fetches: Vec::new(),
                next_fetch_handle: 1,
                temp_ret0: 0,
                function_pointers: vec![None; 14],
                emval: EmvalState::default(),
                location: CmgLocation::from_href(page_url),
            },
        );
        let memory = Memory::new(
            &mut store,
            wasmtime::MemoryType::new(INITIAL_PAGES, Some(MAX_PAGES)),
        )?;
        let table = Table::new(
            &mut store,
            wasmtime::TableType::new(wasmtime::RefType::FUNCREF, TABLE_MIN, None),
            Ref::Func(None),
        )?;
        store.data_mut().memory = Some(memory);
        store.data_mut().table = Some(table);
        {
            let data = memory.data_mut(&mut store);
            write_i32_le_raw(data, DYNAMICTOP_PTR, DYNAMIC_TOP_AFTER_RUNTIME_ALLOCS)?;
        }

        let mut linker = Linker::new(engine);
        linker.define(&mut store, "env", "memory", memory)?;
        linker.define(&mut store, "env", "table", table)?;
        let i32_global = wasmtime::GlobalType::new(
            wasmtime::ValType::I32,
            wasmtime::Mutability::Const,
        );
        let table_base =
            wasmtime::Global::new(&mut store, i32_global.clone(), 0i32.into())?;
        let temp_double_ptr = wasmtime::Global::new(
            &mut store,
            i32_global.clone(),
            TEMP_DOUBLE_PTR.into(),
        )?;
        let dynamic_top_ptr =
            wasmtime::Global::new(&mut store, i32_global.clone(), DYNAMICTOP_PTR.into())?;
        let emt_stack_top =
            wasmtime::Global::new(&mut store, i32_global.clone(), EMT_STACK_TOP.into())?;
        let eb = wasmtime::Global::new(&mut store, i32_global, EB.into())?;
        linker.define(&mut store, "env", "__table_base", table_base)?;
        linker.define(&mut store, "env", "a", temp_double_ptr)?;
        linker.define(&mut store, "env", "b", dynamic_top_ptr)?;
        linker.define(&mut store, "env", "c", emt_stack_top)?;
        linker.define(&mut store, "env", "d", eb)?;
        define_imports(&mut linker)?;

        let instance = linker.instantiate(&mut store, module)?;
        {
            let data = memory.data_mut(&mut store);
            write_i32_le_raw(data, DYNAMICTOP_PTR, DYNAMIC_TOP_AFTER_RUNTIME_ALLOCS)?;
            let static_data = image.static_data();
            let start = EB as usize;
            let end = start + static_data.len();
            data.get_mut(start..end)
                .ok_or_else(|| {
                    wasm_err!("CMG static data out of range len={}", static_data.len())
                })?
                .copy_from_slice(static_data);
        }
        let init_player = instance.get_typed_func::<i32, i32>(&mut store, "ba")?;
        let update_player = instance.get_typed_func::<i32, i32>(&mut store, "da")?;
        let dec_live = ["ea", "fa", "ga", "ha", "ia", "ja", "ka", "la", "ma"]
            .into_iter()
            .map(|name| {
                instance.get_typed_func::<(i32, i32, i32, i32), i32>(&mut store, name)
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let js_malloc = instance.get_typed_func::<i32, i32>(&mut store, "Ca")?;
        let js_free = instance.get_typed_func::<i32, ()>(&mut store, "Ba")?;
        let dyn_call_vi = instance.get_typed_func::<(i32, i32), ()>(&mut store, "Ha")?;
        let malloc = instance.get_typed_func::<i32, i32>(&mut store, "Ea")?;
        store.data_mut().malloc = Some(malloc);
        let free = instance.get_typed_func::<i32, ()>(&mut store, "Aa")?;
        store.data_mut().free = Some(free);
        let global_ctors = instance.get_typed_func::<(), ()>(&mut store, "La")?;
        global_ctors.call(&mut store, ())?;
        call_emscripten_main(&mut store, memory, &instance)?;
        Ok(Self {
            image: Arc::clone(image),
            store,
            memory,
            init_player,
            update_player,
            dec_live,
            js_malloc,
            js_free,
            dyn_call_vi,
            vmp_tag: String::new(),
            update_calls: 0,
            live_export_calls: 0,
            module_dec_calls: 0,
        })
    }

    /// Initializes the player for `media_tag_id` and runs the fetches it queues.
    ///
    /// # Errors
    /// Returns [`crate::WasmError`] when a guest call fails.
    pub fn prime(&mut self, media_tag_id: &str) -> Result<i32> {
        let ptr = self.alloc_zeroed(media_tag_id.len() + MEMORY_EXTEND)?;
        self.write_bytes(ptr, media_tag_id.as_bytes())?;
        let ret = self.init_player.call(&mut self.store, ptr)?;
        self.drain_pending_fetches()?;
        self.free(ptr);
        Ok(ret)
    }

    /// Advances the player state and records the returned player tag.
    ///
    /// # Errors
    /// Returns [`crate::WasmError`] when a guest call fails.
    pub fn update(&mut self, media_tag_id: &str) -> Result<i32> {
        self.update_calls += 1;
        let ptr = self.alloc_zeroed(media_tag_id.len() + MEMORY_EXTEND)?;
        self.write_bytes(ptr, media_tag_id.as_bytes())?;
        let ret = self.update_player.call(&mut self.store, ptr)?;
        if ret != 0 {
            self.vmp_tag = format!("{:08x}", ret as u32);
        }
        self.free(ptr);
        Ok(ret)
    }

    /// Decodes one H.264 NAL with the live export chain.
    ///
    /// # Errors
    /// Returns [`crate::WasmError`] when the WASM call fails or reports a negative
    /// length.
    pub fn module_dec_live(
        &mut self,
        media_tag_id: &str,
        input: &[u8],
        active_url: &str,
    ) -> Result<Vec<u8>> {
        self.module_dec_calls += 1;
        let data_len = input.len() + MEMORY_EXTEND;
        let data_ptr = self.alloc(data_len)?;
        self.write_zeroed(data_ptr, data_len)?;
        self.write_bytes(data_ptr, input)?;
        let active_url_len = if active_url.is_empty() {
            0
        } else {
            self.write_bytes(data_ptr + input.len() as i32, active_url.as_bytes())?;
            active_url.len() as i32
        };
        let key_ptr = self.alloc(media_tag_id.len())?;
        self.write_bytes(key_ptr, media_tag_id.as_bytes())?;

        let call_result = (|| -> Result<i32> {
            let vmp_tag = self.vmp_tag.clone();
            for (position, ch) in vmp_tag.chars().enumerate().take(8) {
                if matches!(ch, '0'..='6') {
                    let live_index = 7usize.saturating_sub(position);
                    self.call_live_export(
                        live_index,
                        key_ptr,
                        data_ptr,
                        input.len() as i32,
                        active_url_len,
                    )?;
                }
            }
            self.call_live_export(
                8,
                key_ptr,
                data_ptr,
                input.len() as i32,
                active_url_len,
            )
        })();
        let output = match call_result {
            Ok(out_len) if out_len >= 0 => self.read_bytes(data_ptr, out_len as usize),
            Ok(out_len) => Err(wasm_err!(
                "CMG moduleDecData live returned negative length: {out_len}"
            )),
            Err(error) => Err(error),
        };
        self.free(data_ptr);
        self.free(key_ptr);
        output
    }

    /// Player tag from the most recent non-zero `update`, as 8 hex digits.
    pub fn vmp_tag(&self) -> &str {
        &self.vmp_tag
    }

    fn call_live_export(
        &mut self,
        live_index: usize,
        key_ptr: i32,
        data_ptr: i32,
        input_len: i32,
        active_url_len: i32,
    ) -> Result<i32> {
        self.live_export_calls += 1;
        let func = self.dec_live.get(live_index).cloned().ok_or_else(|| {
            wasm_err!("CMG live export index out of range: {live_index}")
        })?;
        Ok(func.call(
            &mut self.store,
            (key_ptr, data_ptr, input_len, active_url_len),
        )?)
    }

    fn alloc_zeroed(&mut self, len: usize) -> Result<i32> {
        let ptr = self.alloc(len)?;
        let data = self.memory.data_mut(&mut self.store);
        let start = ptr as usize;
        let end = start + len;
        data.get_mut(start..end)
            .ok_or_else(|| wasm_err!("CMG allocation out of range ptr={ptr} len={len}"))?
            .fill(0);
        Ok(ptr)
    }

    fn alloc(&mut self, len: usize) -> Result<i32> {
        let ptr = self.js_malloc.call(&mut self.store, len as i32)?;
        if ptr <= 0 {
            return Err(wasm_err!("CMG jsmalloc failed for {len} bytes"));
        }
        let data_len = self.memory.data_size(&self.store);
        let start = ptr as usize;
        let end = start + len;
        if end > data_len {
            return Err(wasm_err!(
                "CMG allocation out of range ptr={ptr} len={len} memory={data_len}"
            ));
        }
        Ok(ptr)
    }

    fn free(&mut self, ptr: i32) {
        // A failed free only leaks inside this instance's own linear memory.
        let _ = self.js_free.call(&mut self.store, ptr);
    }

    fn drain_pending_fetches(&mut self) -> Result<()> {
        while let Some(fetch) = self.store.data_mut().pending_fetches.pop() {
            self.complete_fetch(fetch)?;
        }
        Ok(())
    }

    fn complete_fetch(&mut self, fetch: PendingFetch) -> Result<()> {
        if fetch.url.contains("/Library/CMGPlayer.json") {
            let image = Arc::clone(&self.image);
            let payload = image.player_json();
            let data_ptr = self.js_malloc.call(&mut self.store, payload.len() as i32)?;
            self.write_bytes(data_ptr, payload)?;
            {
                let data = self.memory.data_mut(&mut self.store);
                write_i32_le_raw(data, fetch.ptr + 12, data_ptr)?;
                write_u64_le_raw(data, fetch.ptr + 16, payload.len() as u64)?;
                write_u64_le_raw(data, fetch.ptr + 24, 0)?;
                write_u64_le_raw(data, fetch.ptr + 32, payload.len() as u64)?;
                write_u16_le_raw(data, fetch.ptr + 40, 4)?;
                write_u16_le_raw(data, fetch.ptr + 42, 200)?;
                write_c_string_raw(data, fetch.ptr + 44, 64, "OK")?;
            }
            if fetch.onsuccess != 0 {
                self.dyn_call_vi
                    .call(&mut self.store, (fetch.onsuccess, fetch.ptr))?;
            }
            return Ok(());
        }

        {
            let data = self.memory.data_mut(&mut self.store);
            write_i32_le_raw(data, fetch.ptr + 12, 0)?;
            write_u64_le_raw(data, fetch.ptr + 16, 0)?;
            write_u64_le_raw(data, fetch.ptr + 24, 0)?;
            write_u64_le_raw(data, fetch.ptr + 32, 0)?;
            write_u16_le_raw(data, fetch.ptr + 40, 4)?;
            write_u16_le_raw(data, fetch.ptr + 42, 404)?;
            write_c_string_raw(data, fetch.ptr + 44, 64, "Not Found")?;
        }
        if fetch.onerror != 0 {
            self.dyn_call_vi
                .call(&mut self.store, (fetch.onerror, fetch.ptr))?;
        }
        Ok(())
    }

    fn write_bytes(&mut self, ptr: i32, bytes: &[u8]) -> Result<()> {
        let data = self.memory.data_mut(&mut self.store);
        let start = ptr as usize;
        let end = start + bytes.len();
        data.get_mut(start..end)
            .ok_or_else(|| {
                wasm_err!("CMG write out of range ptr={ptr} len={}", bytes.len())
            })?
            .copy_from_slice(bytes);
        Ok(())
    }

    fn write_zeroed(&mut self, ptr: i32, len: usize) -> Result<()> {
        let data = self.memory.data_mut(&mut self.store);
        let start = ptr as usize;
        let end = start + len;
        data.get_mut(start..end)
            .ok_or_else(|| wasm_err!("CMG zero write out of range ptr={ptr} len={len}"))?
            .fill(0);
        Ok(())
    }

    fn read_bytes(&self, ptr: i32, len: usize) -> Result<Vec<u8>> {
        let data = self.memory.data(&self.store);
        let start = ptr as usize;
        let end = start + len;
        Ok(data
            .get(start..end)
            .ok_or_else(|| wasm_err!("CMG read out of range ptr={ptr} len={len}"))?
            .to_vec())
    }
}

fn call_emscripten_main(
    store: &mut Store<CmgState>,
    memory: Memory,
    instance: &wasmtime::Instance,
) -> Result<()> {
    let stack_alloc = instance.get_typed_func::<i32, i32>(&mut *store, "Ma")?;
    let main_func = instance.get_typed_func::<(i32, i32), i32>(&mut *store, "Da")?;
    let program = stack_c_string(store, memory, &stack_alloc, "./this.program")?;
    let argv = stack_alloc.call(&mut *store, 8)?;
    {
        let data = memory.data_mut(&mut *store);
        write_i32_le_raw(data, argv, program)?;
        write_i32_le_raw(data, argv + 4, 0)?;
    }
    main_func.call(store, (1, argv))?;
    Ok(())
}

fn stack_c_string(
    store: &mut Store<CmgState>,
    memory: Memory,
    stack_alloc: &wasmtime::TypedFunc<i32, i32>,
    value: &str,
) -> Result<i32> {
    let ptr = stack_alloc.call(&mut *store, (value.len() + 1) as i32)?;
    let data = memory.data_mut(store);
    let start = ptr as usize;
    data.get_mut(start..start + value.len())
        .ok_or_else(|| wasm_err!("CMG stack string write out of range"))?
        .copy_from_slice(value.as_bytes());
    *data
        .get_mut(start + value.len())
        .ok_or_else(|| wasm_err!("CMG stack string terminator out of range"))? = 0;
    Ok(ptr)
}

fn define_imports(linker: &mut Linker<CmgState>) -> Result<()> {
    linker.func_wrap("env", "e", |caller: Caller<'_, CmgState>| -> i32 {
        caller.data().temp_ret0
    })?;
    linker.func_wrap(
        "env",
        "f",
        |mut caller: Caller<'_, CmgState>, value: i32| {
            caller.data_mut().temp_ret0 = value;
        },
    )?;
    linker.func_wrap(
        "env",
        "g",
        |mut caller: Caller<'_, CmgState>, ptr: i32, _tz: i32| -> i32 {
            let now_ms = now_ms_f64().max(0.0) as u64;
            let _ = write_i32(&mut caller, ptr, (now_ms / 1000) as i32);
            let _ = write_i32(&mut caller, ptr + 4, ((now_ms % 1000) * 1000) as i32);
            0
        },
    )?;
    linker.func_wrap(
        "env",
        "h",
        |_db: i32, _fetch: i32, _data: i32, _onsuccess: i32, _onerror: i32| {},
    )?;
    linker.func_wrap("env", "i", || -> f64 { now_ms_f64() })?;
    linker.func_wrap("env", "j", || {})?;
    linker.func_wrap(
        "env",
        "k",
        |mut caller: Caller<'_, CmgState>, func: i32, a: i32| {
            let _ = call_table_v(&mut caller, func, &[a]);
        },
    )?;
    linker.func_wrap("env", "l", |_func: i32| {})?;
    linker.func_wrap(
        "env",
        "m",
        |_func: i32, _a: i32, _b: i32, _c: i32, _d: i32| -> i32 { 0 },
    )?;
    linker.func_wrap("env", "n", |_func: i32, _a: i32, _b: i32, _c: i32| -> i32 {
        0
    })?;
    linker.func_wrap("env", "o", |_requested_size: i32| -> i32 { 0 })?;
    linker.func_wrap("env", "p", |_func: i32, _a: i32, _b: i32| -> i32 { 0 })?;
    linker.func_wrap("env", "q", || {})?;
    linker.func_wrap(
        "env",
        "r",
        |mut caller: Caller<'_, CmgState>, fetch: i32| {
            let _ = emscripten_start_fetch(&mut caller, fetch);
        },
    )?;
    linker.func_wrap(
        "env",
        "s",
        |mut caller: Caller<'_, CmgState>, requested_size: i32| -> i32 {
            resize_heap(&mut caller, requested_size).unwrap_or(0)
        },
    )?;
    linker.func_wrap(
        "env",
        "t",
        |mut caller: Caller<'_, CmgState>, dst: i32, src: i32, len: i32| -> i32 {
            let _ = copy_within_memory(&mut caller, dst, src, len);
            dst
        },
    )?;
    linker.func_wrap(
        "env",
        "u",
        // Guest console output is intentionally dropped.
        |_flags: i32, _varargs: i32| {},
    )?;
    linker.func_wrap("env", "v", || -> i32 { 1 })?;
    linker.func_wrap("env", "w", |caller: Caller<'_, CmgState>| -> i32 {
        caller
            .data()
            .memory
            .map(|memory| memory.data_size(&caller) as i32)
            .unwrap_or(0)
    })?;
    linker.func_wrap(
        "env",
        "x",
        |_flags: i32, _out: i32, _maxbytes: i32| -> i32 { 0 },
    )?;
    linker.func_wrap(
        "env",
        "y",
        |_func: i32, _a: i32, _b: f64, _c: i32, _d: i32, _e: i32, _f: i32| -> i32 { 0 },
    )?;
    linker.func_wrap(
        "env",
        "z",
        |mut caller: Caller<'_, CmgState>, index: i32, arg: i32| -> i32 {
            asm_const_ii(&mut caller, index, arg).unwrap_or(0)
        },
    )?;
    linker.func_wrap("env", "A", || {})?;
    linker.func_wrap(
        "env",
        "B",
        |mut caller: Caller<'_, CmgState>, type_id: i32, ptr: i32| -> i32 {
            emval_take_value(&mut caller, type_id, ptr).unwrap_or(0)
        },
    )?;
    linker.func_wrap(
        "env",
        "C",
        |mut caller: Caller<'_, CmgState>, destructors: i32| {
            emval_run_destructors(&mut caller, destructors);
        },
    )?;
    linker.func_wrap(
        "env",
        "D",
        |mut caller: Caller<'_, CmgState>, obj: i32, prop: i32| -> i32 {
            emval_get_property(&mut caller, obj, prop).unwrap_or(0)
        },
    )?;
    linker.func_wrap(
        "env",
        "E",
        |mut caller: Caller<'_, CmgState>, name: i32| -> i32 {
            emval_get_global(&mut caller, name).unwrap_or(0)
        },
    )?;
    linker.func_wrap(
        "env",
        "F",
        |mut caller: Caller<'_, CmgState>, handle: i32| {
            emval_decref(&mut caller, handle);
        },
    )?;
    linker.func_wrap(
        "env",
        "G",
        |mut caller: Caller<'_, CmgState>,
         handle: i32,
         return_type: i32,
         destructors: i32|
         -> f64 {
            emval_as(&mut caller, handle, return_type, destructors).unwrap_or(0) as f64
        },
    )?;
    linker.func_wrap("env", "H", |_fetch: i32| {})?;
    linker.func_wrap(
        "env",
        "I",
        |mut caller: Caller<'_, CmgState>, func: i32, a: i32| -> i32 {
            call_table_i(&mut caller, func, &[a]).unwrap_or(0)
        },
    )?;
    linker.func_wrap("env", "J", |_raw_type: i32, _name: i32| {})?;
    linker.func_wrap("env", "K", |_raw_type: i32, _char_size: i32, _name: i32| {})?;
    linker.func_wrap(
        "env",
        "L",
        |mut caller: Caller<'_, CmgState>, raw_type: i32, name: i32| {
            if read_c_string(&mut caller, name).ok().as_deref() == Some("std::string") {
                caller.data_mut().emval.std_string_type = Some(raw_type);
            }
        },
    )?;
    linker.func_wrap(
        "env",
        "M",
        |_raw_type: i32, _data_type_index: i32, _name: i32| {},
    )?;
    linker.func_wrap(
        "env",
        "N",
        |_raw_type: i32, _name: i32, _size: i32, _min: i32, _max: i32| {},
    )?;
    linker.func_wrap("env", "O", |_raw_type: i32, _name: i32, _size: i32| {})?;
    linker.func_wrap("env", "P", |_raw_type: i32, _name: i32| {})?;
    linker.func_wrap(
        "env",
        "Q",
        |_raw_type: i32, _name: i32, _size: i32, _true_value: i32, _false_value: i32| {},
    )?;
    linker.func_wrap("env", "R", |_which: i32, _varargs: i32| -> i32 { 0 })?;
    linker.func_wrap("env", "S", |_buf: i32, _len: i32| -> i32 { 0 })?;
    linker.func_wrap("env", "T", |_which: i32, _varargs: i32| -> i32 { 0 })?;
    linker.func_wrap("env", "U", |_errno: i32| {})?;
    linker.func_wrap("env", "V", || -> i32 { 0 })?;
    linker.func_wrap("env", "W", |_ptr: i32| -> i32 { 0 })?;
    linker.func_wrap(
        "env",
        "X",
        |mut caller: Caller<'_, CmgState>,
         func: i32,
         a: i32,
         b: i32,
         c: i32,
         d: i32,
         e: i32,
         f: i32| {
            let _ = call_table_v(&mut caller, func, &[a, b, c, d, e, f]);
        },
    )?;
    linker.func_wrap(
        "env",
        "Y",
        |mut caller: Caller<'_, CmgState>,
         func: i32,
         a: i32,
         b: i32,
         c: i32,
         d: i32,
         e: i32| {
            let _ = call_table_v(&mut caller, func, &[a, b, c, d, e]);
        },
    )?;
    linker.func_wrap(
        "env",
        "Z",
        |mut caller: Caller<'_, CmgState>, func: i32, a: i32, b: i32, c: i32, d: i32| {
            let _ = call_table_v(&mut caller, func, &[a, b, c, d]);
        },
    )?;
    linker.func_wrap(
        "env",
        "_",
        |mut caller: Caller<'_, CmgState>, func: i32, a: i32, b: i32, c: i32| {
            let _ = call_table_v(&mut caller, func, &[a, b, c]);
        },
    )?;
    linker.func_wrap(
        "env",
        "$",
        |mut caller: Caller<'_, CmgState>, func: i32, a: i32, b: i32| {
            let _ = call_table_v(&mut caller, func, &[a, b]);
        },
    )?;
    linker.func_wrap(
        "env",
        "aa",
        |mut caller: Caller<'_, CmgState>, ptr: i32| -> wasmtime::Result<()> {
            let message =
                read_c_string(&mut caller, ptr).unwrap_or_else(|_| "abort".to_string());
            Err(wasmtime::Error::msg(format!("CMG abort: {message}")))
        },
    )?;
    Ok(())
}

fn asm_const_ii(caller: &mut Caller<'_, CmgState>, index: i32, arg: i32) -> Result<i32> {
    if index != 0 {
        return Ok(0);
    }
    let expr = read_c_string(caller, arg)?;
    let location = caller.data().location.clone();
    let value = match expr.as_str() {
        "location.href" => location.href.clone(),
        "self.location.href" => location.href.clone(),
        "window.location.href" => location.href.clone(),
        "location.host" => location.host.clone(),
        "self.location.host" => location.host.clone(),
        "window.location.host" => location.host.clone(),
        "location.hostname" => location.hostname.clone(),
        "self.location.hostname" => location.hostname.clone(),
        "window.location.hostname" => location.hostname.clone(),
        "location.origin" => location.origin.clone(),
        "self.location.origin" => location.origin.clone(),
        "window.location.origin" => location.origin.clone(),
        "location.protocol" => location.protocol.clone(),
        "self.location.protocol" => location.protocol.clone(),
        "window.location.protocol" => location.protocol.clone(),
        "document.URL" => location.href.clone(),
        _ => String::new(),
    };
    malloc_c_string(caller, &value)
}

fn emscripten_start_fetch(caller: &mut Caller<'_, CmgState>, fetch: i32) -> Result<i32> {
    let memory = cmg_memory(caller)?;
    let (url, onsuccess, onerror) = {
        let data = memory.data(&mut *caller);
        let url_ptr = read_u32_le_raw(data, fetch + 8)? as i32;
        let attr = fetch + 112;
        (
            read_c_string_from_memory(data, url_ptr)?,
            read_u32_le_raw(data, attr + 36)? as i32,
            read_u32_le_raw(data, attr + 40)? as i32,
        )
    };
    let handle = {
        let state = caller.data_mut();
        let handle = state.next_fetch_handle.max(1);
        state.next_fetch_handle = handle + 1;
        state.pending_fetches.push(PendingFetch {
            ptr: fetch,
            url: url.clone(),
            onsuccess,
            onerror,
        });
        handle
    };
    {
        let data = memory.data_mut(caller);
        write_i32_le_raw(data, fetch, handle)?;
    }
    Ok(fetch)
}

fn emval_register(caller: &mut Caller<'_, CmgState>, value: EmvalValue) -> i32 {
    match value {
        EmvalValue::Undefined => 1,
        EmvalValue::Null => 2,
        EmvalValue::Bool(true) => 3,
        EmvalValue::Bool(false) => 4,
        other => {
            let state = &mut caller.data_mut().emval;
            if let Some(handle) = state.free.pop() {
                let idx = handle as usize;
                if idx >= state.handles.len() {
                    state.handles.resize(idx + 1, None);
                }
                if let Some(slot) = state.handles.get_mut(idx) {
                    *slot = Some(other);
                }
                handle
            } else {
                state.handles.push(Some(other));
                (state.handles.len() - 1) as i32
            }
        }
    }
}

fn emval_value(caller: &Caller<'_, CmgState>, handle: i32) -> Option<EmvalValue> {
    caller
        .data()
        .emval
        .handles
        .get(handle as usize)
        .and_then(Clone::clone)
}

fn emval_decref(caller: &mut Caller<'_, CmgState>, handle: i32) {
    if handle <= 4 {
        return;
    }
    let state = &mut caller.data_mut().emval;
    let idx = handle as usize;
    if state.handles.get(idx).is_some_and(Option::is_some) {
        if let Some(slot) = state.handles.get_mut(idx) {
            *slot = None;
        }
        state.free.push(handle);
        if state.location_handle == Some(handle) {
            state.location_handle = None;
        }
    }
}

fn emval_get_global(caller: &mut Caller<'_, CmgState>, name_ptr: i32) -> Result<i32> {
    let name = if name_ptr == 0 {
        String::new()
    } else {
        read_c_string(caller, name_ptr)?
    };
    if name == "location" {
        if let Some(handle) = caller.data().emval.location_handle {
            if emval_value(caller, handle).is_some() {
                return Ok(handle);
            }
        }
        let handle = emval_register(caller, EmvalValue::Location);
        caller.data_mut().emval.location_handle = Some(handle);
        return Ok(handle);
    }
    Ok(emval_register(caller, EmvalValue::Undefined))
}

fn emval_take_value(
    caller: &mut Caller<'_, CmgState>,
    type_id: i32,
    ptr: i32,
) -> Result<i32> {
    let value = if caller.data().emval.std_string_type == Some(type_id) {
        read_std_string(caller, ptr)?
    } else {
        String::new()
    };
    Ok(emval_register(caller, EmvalValue::String(value)))
}

fn emval_get_property(
    caller: &mut Caller<'_, CmgState>,
    obj: i32,
    prop: i32,
) -> Result<i32> {
    let object = emval_value(caller, obj).unwrap_or(EmvalValue::Undefined);
    let property = emval_value(caller, prop).unwrap_or(EmvalValue::Undefined);
    let value = match (object, property) {
        (EmvalValue::Location, EmvalValue::String(name)) => match name.as_str() {
            "host" => EmvalValue::String(caller.data().location.host.clone()),
            "protocol" => EmvalValue::String(caller.data().location.protocol.clone()),
            "href" => EmvalValue::String(caller.data().location.href.clone()),
            "hostname" => EmvalValue::String(caller.data().location.hostname.clone()),
            "origin" => EmvalValue::String(caller.data().location.origin.clone()),
            _ => EmvalValue::Undefined,
        },
        _ => EmvalValue::Undefined,
    };
    Ok(emval_register(caller, value))
}

fn emval_as(
    caller: &mut Caller<'_, CmgState>,
    handle: i32,
    return_type: i32,
    destructors_ptr: i32,
) -> Result<i32> {
    if caller.data().emval.std_string_type != Some(return_type) {
        return Ok(0);
    }
    let value = match emval_value(caller, handle).unwrap_or(EmvalValue::Undefined) {
        EmvalValue::String(value) => value,
        EmvalValue::Bool(value) => {
            if value {
                "true".to_string()
            } else {
                "false".to_string()
            }
        }
        EmvalValue::Null => "null".to_string(),
        EmvalValue::Undefined => String::new(),
        EmvalValue::Location => "location".to_string(),
        EmvalValue::Destructors(_) => String::new(),
    };
    let wire = malloc_std_string(caller, &value)?;
    let destructors = emval_register(caller, EmvalValue::Destructors(vec![wire]));
    write_i32(caller, destructors_ptr, destructors)?;
    Ok(wire)
}

fn emval_run_destructors(caller: &mut Caller<'_, CmgState>, destructors: i32) {
    if let Some(EmvalValue::Destructors(ptrs)) = emval_value(caller, destructors) {
        for ptr in ptrs {
            let _ = free_std_string(caller, ptr);
        }
    }
    emval_decref(caller, destructors);
}

fn read_std_string(caller: &mut Caller<'_, CmgState>, ptr: i32) -> Result<String> {
    let memory = cmg_memory(caller)?;
    let (string_ptr, bytes) = {
        let data = memory.data(&mut *caller);
        let string_ptr = read_u32_le_raw(data, ptr)? as i32;
        let len = read_u32_le_raw(data, string_ptr)? as usize;
        let start = string_ptr as usize + 4;
        let end = start + len;
        let bytes = data
            .get(start..end)
            .ok_or_else(|| {
                wasm_err!(
                    "CMG std::string read out of range ptr={ptr} string_ptr={string_ptr} len={len}"
                )
            })?
            .to_vec();
        (string_ptr, bytes)
    };
    let value = String::from_utf8(bytes).context("CMG std::string is not utf8")?;
    free_std_string(caller, string_ptr)?;
    Ok(value)
}

fn malloc_std_string(caller: &mut Caller<'_, CmgState>, value: &str) -> Result<i32> {
    let malloc = caller
        .data()
        .malloc
        .clone()
        .ok_or_else(|| wasm_err!("CMG malloc export missing"))?;
    let ptr = malloc.call(&mut *caller, (4 + value.len() + 1) as i32)?;
    let memory = cmg_memory(caller)?;
    let data = memory.data_mut(caller);
    write_i32_le_raw(data, ptr, value.len() as i32)?;
    let start = ptr as usize + 4;
    data.get_mut(start..start + value.len())
        .ok_or_else(|| wasm_err!("CMG malloc std::string write out of range"))?
        .copy_from_slice(value.as_bytes());
    *data
        .get_mut(start + value.len())
        .ok_or_else(|| wasm_err!("CMG std::string terminator out of range"))? = 0;
    Ok(ptr)
}

fn free_std_string(caller: &mut Caller<'_, CmgState>, ptr: i32) -> Result<()> {
    if ptr <= 0 {
        return Ok(());
    }
    let free = caller
        .data()
        .free
        .clone()
        .ok_or_else(|| wasm_err!("CMG free export missing"))?;
    free.call(caller, ptr)?;
    Ok(())
}

fn call_table_i(
    caller: &mut Caller<'_, CmgState>,
    func_index: i32,
    args: &[i32],
) -> Result<i32> {
    let table = caller
        .data()
        .table
        .ok_or_else(|| wasm_err!("CMG table missing"))?;
    let idx = map_func_index(caller, func_index)?;
    let Some(Ref::Func(Some(func))) = table.get(&mut *caller, u64::from(idx)) else {
        return Ok(0);
    };
    match *args {
        [a] => Ok(func.typed::<i32, i32>(&caller)?.call(caller, a)?),
        [a, b] => Ok(func
            .typed::<(i32, i32), i32>(&caller)?
            .call(caller, (a, b))?),
        [a, b, c] => Ok(func
            .typed::<(i32, i32, i32), i32>(&caller)?
            .call(caller, (a, b, c))?),
        _ => Ok(0),
    }
}

fn call_table_v(
    caller: &mut Caller<'_, CmgState>,
    func_index: i32,
    args: &[i32],
) -> Result<()> {
    let table = caller
        .data()
        .table
        .ok_or_else(|| wasm_err!("CMG table missing"))?;
    let idx = map_func_index(caller, func_index)?;
    let Some(Ref::Func(Some(func))) = table.get(&mut *caller, u64::from(idx)) else {
        return Ok(());
    };
    match *args {
        [] => func.typed::<(), ()>(&caller)?.call(caller, ())?,
        [a] => func.typed::<i32, ()>(&caller)?.call(caller, a)?,
        [a, b] => func
            .typed::<(i32, i32), ()>(&caller)?
            .call(caller, (a, b))?,
        [a, b, c] => func
            .typed::<(i32, i32, i32), ()>(&caller)?
            .call(caller, (a, b, c))?,
        [a, b, c, d] => func
            .typed::<(i32, i32, i32, i32), ()>(&caller)?
            .call(caller, (a, b, c, d))?,
        [a, b, c, d, e] => func
            .typed::<(i32, i32, i32, i32, i32), ()>(&caller)?
            .call(caller, (a, b, c, d, e))?,
        [a, b, c, d, e, f] => func
            .typed::<(i32, i32, i32, i32, i32, i32), ()>(&caller)?
            .call(caller, (a, b, c, d, e, f))?,
        _ => {}
    }
    Ok(())
}

fn map_func_index(caller: &Caller<'_, CmgState>, func_index: i32) -> Result<u32> {
    Ok(map_func_index_from_state(caller.data(), func_index))
}

fn map_func_index_from_state(state: &CmgState, func_index: i32) -> u32 {
    if func_index >= 1 {
        let idx = (func_index - 1) as usize;
        if let Some(Some(value)) = state.function_pointers.get(idx) {
            return *value;
        }
    }
    func_index as u32
}

fn resize_heap(caller: &mut Caller<'_, CmgState>, requested_size: i32) -> Result<i32> {
    let memory = caller
        .data()
        .memory
        .ok_or_else(|| wasm_err!("CMG memory missing"))?;
    let current = memory.data_size(&mut *caller);
    if requested_size as usize <= current {
        return Ok(1);
    }
    let page = 65_536usize;
    let wanted_pages = (requested_size as usize).div_ceil(page);
    let current_pages = current / page;
    if wanted_pages > current_pages {
        memory.grow(caller, (wanted_pages - current_pages) as u64)?;
    }
    Ok(1)
}

fn malloc_c_string(caller: &mut Caller<'_, CmgState>, value: &str) -> Result<i32> {
    let len = value.len() + 1;
    let malloc = caller
        .data()
        .malloc
        .clone()
        .ok_or_else(|| wasm_err!("CMG malloc export missing"))?;
    let ptr = malloc.call(&mut *caller, len as i32)?;
    let memory = cmg_memory(caller)?;
    let data = memory.data_mut(caller);
    let start = ptr as usize;
    data.get_mut(start..start + value.len())
        .ok_or_else(|| wasm_err!("CMG malloc c-string allocation out of range"))?
        .copy_from_slice(value.as_bytes());
    *data
        .get_mut(start + value.len())
        .ok_or_else(|| wasm_err!("CMG c-string terminator out of range"))? = 0;
    Ok(ptr)
}

fn cmg_memory(caller: &mut Caller<'_, CmgState>) -> Result<Memory> {
    caller
        .data()
        .memory
        .ok_or_else(|| wasm_err!("CMG memory not initialized"))
}

fn write_i32(caller: &mut Caller<'_, CmgState>, ptr: i32, value: i32) -> Result<()> {
    let memory = cmg_memory(caller)?;
    let data = memory.data_mut(caller);
    write_i32_le_raw(data, ptr, value)
}

fn write_i32_le_raw(data: &mut [u8], ptr: i32, value: i32) -> Result<()> {
    let start = ptr as usize;
    data.get_mut(start..start + 4)
        .ok_or_else(|| wasm_err!("CMG i32 write out of range ptr={ptr}"))?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn write_u16_le_raw(data: &mut [u8], ptr: i32, value: u16) -> Result<()> {
    let start = ptr as usize;
    data.get_mut(start..start + 2)
        .ok_or_else(|| wasm_err!("CMG u16 write out of range ptr={ptr}"))?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn write_u64_le_raw(data: &mut [u8], ptr: i32, value: u64) -> Result<()> {
    let start = ptr as usize;
    data.get_mut(start..start + 8)
        .ok_or_else(|| wasm_err!("CMG u64 write out of range ptr={ptr}"))?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn read_u32_le_raw(data: &[u8], ptr: i32) -> Result<u32> {
    let start = ptr as usize;
    let bytes = data
        .get(start..start + 4)
        .ok_or_else(|| wasm_err!("CMG u32 read out of range ptr={ptr}"))?;
    Ok(u32::from_le_bytes(<[u8; 4]>::try_from(bytes)?))
}

fn write_c_string_raw(
    data: &mut [u8],
    ptr: i32,
    max_len: usize,
    value: &str,
) -> Result<()> {
    let start = ptr as usize;
    let end = start + max_len;
    let target = data.get_mut(start..end).ok_or_else(|| {
        wasm_err!("CMG c-string raw write out of range ptr={ptr} len={max_len}")
    })?;
    target.fill(0);
    let len = value.len().min(max_len.saturating_sub(1));
    if let (Some(dst), Some(src)) = (target.get_mut(..len), value.as_bytes().get(..len)) {
        dst.copy_from_slice(src);
    }
    Ok(())
}

fn copy_within_memory(
    caller: &mut Caller<'_, CmgState>,
    dst: i32,
    src: i32,
    len: i32,
) -> Result<()> {
    if len <= 0 {
        return Ok(());
    }
    let memory = cmg_memory(caller)?;
    let data = memory.data_mut(caller);
    let src_start = src as usize;
    let src_end = src_start + len as usize;
    let dst_start = dst as usize;
    data.copy_within(src_start..src_end, dst_start);
    Ok(())
}

fn read_c_string(caller: &mut Caller<'_, CmgState>, ptr: i32) -> Result<String> {
    if ptr == 0 {
        return Ok(String::new());
    }
    let memory = cmg_memory(caller)?;
    let data = memory.data(caller);
    read_c_string_from_memory(data, ptr)
}

fn read_c_string_from_memory(data: &[u8], ptr: i32) -> Result<String> {
    if ptr == 0 {
        return Ok(String::new());
    }
    let start = ptr as usize;
    let tail = data
        .get(start..)
        .ok_or_else(|| wasm_err!("CMG c-string pointer out of range ptr={ptr}"))?;
    let len = tail
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(tail.len());
    let bytes = tail.get(..len).unwrap_or_default();
    String::from_utf8(bytes.to_vec()).context("CMG c-string is not utf8")
}

fn now_duration() -> std::time::Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}

fn now_ms_f64() -> f64 {
    now_duration().as_millis() as f64
}
