use std::collections::HashMap;

use wasmtime::{Caller, Linker, Memory, Module, Store};

use crate::error::{Context, Result};

struct WasmState {
    values: HashMap<String, String>,
    heap: Vec<Option<String>>,
}

impl WasmState {
    fn from_input(state: &KeygenInput) -> Self {
        let values = HashMap::from([
            ("cctvh5openapi.state.guid".to_string(), state.guid.clone()),
            (
                "cctvh5openapi.state.yspappid".to_string(),
                state.app_id.clone(),
            ),
            (
                "cctvh5openapi.state.version".to_string(),
                state.version.clone(),
            ),
            ("window.location.host".to_string(), state.host.clone()),
            (
                "window.location.protocol".to_string(),
                state.protocol.clone(),
            ),
            ("cctvh5openapi.state.token".to_string(), state.token.clone()),
            ("cctvh5openapi.state.input".to_string(), state.input.clone()),
            ("cctvh5openapi.state.ts".to_string(), state.ts.clone()),
        ]);
        // Slots 129..=131 hold the well-known JS values null, true and false.
        let mut heap = vec![None; 129];
        heap.extend(["null", "true", "false"].map(|value| Some(value.to_string())));
        Self { values, heap }
    }

    fn add_heap_string(&mut self, value: String) -> i32 {
        self.heap.push(Some(value));
        (self.heap.len() - 1) as i32
    }

    fn heap_string(&self, idx: i32) -> Option<String> {
        self.heap.get(idx as usize).and_then(Clone::clone)
    }

    fn drop_heap(&mut self, idx: i32) {
        if idx >= 132 {
            if let Some(slot) = self.heap.get_mut(idx as usize) {
                *slot = None;
            }
        }
    }
}

/// State the keygen guest reads through its imports.
#[derive(Debug, Clone)]
pub struct KeygenInput {
    /// Client guid.
    pub guid: String,
    /// Access token (empty when requesting the token itself).
    pub token: String,
    /// Application id.
    pub app_id: String,
    /// Signing input string.
    pub input: String,
    /// Timestamp in milliseconds.
    pub ts: String,
    /// SDK version.
    pub version: String,
    /// Page host.
    pub host: String,
    /// Page protocol, e.g. `https:`.
    pub protocol: String,
}

/// Runs the keygen module for SDK signatures and token challenges.
#[derive(Clone)]
pub struct KeygenSigner {
    module: Module,
}

impl KeygenSigner {
    pub(crate) fn new(module: Module) -> Self {
        Self { module }
    }

    /// Returns the hex signature for `input`. Each call uses a fresh instance.
    ///
    /// # Errors
    /// Returns [`crate::WasmError`] when instantiation or the guest call fails.
    pub fn signature_hex(&self, input: KeygenInput) -> Result<String> {
        KeygenWasm::load(&self.module, &input)?.get_signature_hex()
    }

    /// Returns the token challenge string for `input`.
    ///
    /// # Errors
    /// Returns [`crate::WasmError`] when instantiation or the guest call fails.
    pub fn token_rnd(&self, input: KeygenInput) -> Result<String> {
        KeygenWasm::load(&self.module, &input)?.get_token_rnd()
    }
}

struct KeygenWasm {
    store: Store<WasmState>,
    memory: Memory,
    get_signature: wasmtime::TypedFunc<i32, ()>,
    get_token_rnd: wasmtime::TypedFunc<i32, ()>,
    add_stack_pointer: wasmtime::TypedFunc<i32, i32>,
    free: wasmtime::TypedFunc<(i32, i32, i32), ()>,
}

impl KeygenWasm {
    fn load(module: &Module, input: &KeygenInput) -> Result<Self> {
        let engine = module.engine();
        let mut linker = Linker::new(engine);

        linker.func_wrap(
            "wbg",
            "__wbg_get_9c1840f7ecd81363",
            |mut caller: Caller<'_, WasmState>,
             ptr: i32,
             len: i32|
             -> wasmtime::Result<i32> {
                let key = read_string(&mut caller, ptr, len)?;
                let value = caller.data().values.get(&key).cloned().unwrap_or_default();
                Ok(caller.data_mut().add_heap_string(value))
            },
        )?;
        linker.func_wrap(
            "wbg",
            "__wbindgen_string_get",
            |mut caller: Caller<'_, WasmState>,
             out_ptr: i32,
             obj_idx: i32|
             -> wasmtime::Result<()> {
                let value = caller.data().heap_string(obj_idx);
                let (ptr, len) = if let Some(value) = value {
                    let bytes = value.as_bytes();
                    let malloc = caller
                        .get_export("__wbindgen_malloc")
                        .and_then(|export| export.into_func())
                        .ok_or_else(|| wasm_err!("missing __wbindgen_malloc export"))?
                        .typed::<(i32, i32), i32>(&caller)?;
                    let ptr = malloc.call(&mut caller, (bytes.len() as i32, 1))?;
                    let memory = memory(&mut caller)?;
                    let data = memory.data_mut(&mut caller);
                    let start = ptr as usize;
                    let end = start + bytes.len();
                    data.get_mut(start..end)
                        .ok_or_else(|| wasm_err!("wasm string allocation out of range"))?
                        .copy_from_slice(bytes);
                    (ptr, bytes.len() as i32)
                } else {
                    (0, 0)
                };
                let memory = memory(&mut caller)?;
                let data = memory.data_mut(&mut caller);
                write_i32_le(data, out_ptr, ptr)?;
                write_i32_le(data, out_ptr + 4, len)?;
                Ok(())
            },
        )?;
        linker.func_wrap(
            "wbg",
            "__wbindgen_object_drop_ref",
            |mut caller: Caller<'_, WasmState>, idx: i32| {
                caller.data_mut().drop_heap(idx);
            },
        )?;

        let mut store = Store::new(engine, WasmState::from_input(input));
        let instance = linker.instantiate(&mut store, module)?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| wasm_err!("missing keygen memory export"))?;
        let get_signature =
            instance.get_typed_func::<i32, ()>(&mut store, "get_signature")?;
        let get_token_rnd =
            instance.get_typed_func::<i32, ()>(&mut store, "get_token_rnd")?;
        let add_stack_pointer = instance
            .get_typed_func::<i32, i32>(&mut store, "__wbindgen_add_to_stack_pointer")?;
        let free = instance
            .get_typed_func::<(i32, i32, i32), ()>(&mut store, "__wbindgen_free")?;
        Ok(Self {
            store,
            memory,
            get_signature,
            get_token_rnd,
            add_stack_pointer,
            free,
        })
    }

    fn get_token_rnd(&mut self) -> Result<String> {
        self.call_string_export(self.get_token_rnd.clone())
    }

    fn get_signature_hex(&mut self) -> Result<String> {
        self.call_string_export(self.get_signature.clone())
    }

    fn call_string_export(
        &mut self,
        func: wasmtime::TypedFunc<i32, ()>,
    ) -> Result<String> {
        let ret_ptr = self.add_stack_pointer.call(&mut self.store, -16)?;
        let mut ptr = 0;
        let mut len = 0;
        let result = (|| -> Result<String> {
            func.call(&mut self.store, ret_ptr)?;
            let data = self.memory.data(&self.store);
            ptr = read_i32_le(data, ret_ptr)?;
            len = read_i32_le(data, ret_ptr + 4)?;
            let start = ptr as usize;
            let end = start + len as usize;
            let bytes = data
                .get(start..end)
                .ok_or_else(|| wasm_err!("wasm return string out of range"))?;
            String::from_utf8(bytes.to_vec()).context("wasm returned non-utf8 string")
        })();
        let _ = self.add_stack_pointer.call(&mut self.store, 16);
        if ptr > 0 && len >= 0 {
            let _ = self.free.call(&mut self.store, (ptr, len, 1));
        }
        result
    }
}

fn memory(caller: &mut Caller<'_, WasmState>) -> Result<Memory> {
    caller
        .get_export("memory")
        .and_then(|export| export.into_memory())
        .ok_or_else(|| wasm_err!("missing wasm memory export"))
}

fn read_string(caller: &mut Caller<'_, WasmState>, ptr: i32, len: i32) -> Result<String> {
    let memory = memory(caller)?;
    let data = memory.data(caller);
    let start = ptr as usize;
    let end = start + len as usize;
    let bytes = data
        .get(start..end)
        .ok_or_else(|| wasm_err!("wasm read string out of range"))?;
    String::from_utf8(bytes.to_vec()).context("wasm input string is non-utf8")
}

fn write_i32_le(data: &mut [u8], ptr: i32, value: i32) -> Result<()> {
    let start = ptr as usize;
    let end = start + 4;
    data.get_mut(start..end)
        .ok_or_else(|| wasm_err!("wasm i32 write out of range"))?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn read_i32_le(data: &[u8], ptr: i32) -> Result<i32> {
    let start = ptr as usize;
    let end = start + 4;
    let bytes = data
        .get(start..end)
        .ok_or_else(|| wasm_err!("wasm i32 read out of range"))?;
    Ok(i32::from_le_bytes(<[u8; 4]>::try_from(bytes)?))
}
