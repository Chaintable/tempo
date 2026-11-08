//! Preserves the deployed RPC call tree across Tempo's top-level AA calls.

use alloy_primitives::{Address, Log, U256};
use evm2::{
    Evm, EvmTypes, Inspector,
    interpreter::{Interpreter, Message, MessageResult},
};
use evm2_inspectors::tracing::{TracingInspector, TracingInspectorConfig};

#[derive(Debug)]
pub(crate) struct RpcTracingInspector {
    pub(crate) inner: TracingInspector,
    entered: bool,
    aa: bool,
    static_precompiles: Vec<Address>,
    active_frames: usize,
}

impl RpcTracingInspector {
    pub(crate) fn new(config: TracingInspectorConfig, aa: bool) -> Self {
        Self {
            inner: TracingInspector::new(config),
            entered: false,
            aa,
            static_precompiles: Vec::new(),
            active_frames: 0,
        }
    }

    pub(crate) fn set_aa(&mut self, aa: bool) {
        debug_assert!(!self.entered);
        self.aa = aa;
    }

    fn enter<T: EvmTypes>(&mut self, interp: &mut Interpreter<'_, '_, T>) {
        if self.entered {
            return;
        }
        self.entered = true;
        if self.inner.config().exclude_precompile_calls {
            // The previous inspector excluded the journal's warm address set,
            // which contained Ethereum precompiles, not Tempo's dynamic lookup.
            self.static_precompiles = interp.host().precompiles().addresses();
        }
        if self.aa {
            // EVM2 replaces an unattached root on every top-level call. Keep a
            // trace-only transaction parent open across all AA calls, as in the
            // previous RPC output. This message is never executed by the EVM.
            let mut root = Message::<T>::default();
            self.inner.call(interp, &mut root);
        }
    }
}

impl<T: EvmTypes> Inspector<T> for RpcTracingInspector {
    fn initialize_interp(&mut self, interp: &mut Interpreter<'_, '_, T>) {
        self.inner.initialize_interp(interp);
    }

    fn step(&mut self, interp: &mut Interpreter<'_, '_, T>) {
        self.inner.step(interp);
    }

    fn step_end(&mut self, interp: &mut Interpreter<'_, '_, T>) {
        self.inner.step_end(interp);
    }

    fn log(&mut self, log: &Log, host: &mut Evm<'_, T>) {
        // Protocol hooks can emit logs outside any EVM message. Such logs have
        // no call-tree parent; the unchanged receipt path retains them and the
        // block RPC reconciles them with its event output after execution.
        if self.active_frames != 0 {
            <TracingInspector as Inspector<T>>::log(&mut self.inner, log, host);
        }
    }

    fn call(
        &mut self,
        interp: &mut Interpreter<'_, '_, T>,
        message: &mut Message<T>,
    ) -> Option<MessageResult<T>> {
        self.enter(interp);
        self.active_frames += 1;
        let exclude_precompiles = self.inner.config().exclude_precompile_calls;
        self.inner.config_mut().exclude_precompile_calls = exclude_precompiles
            && !(self.aa && message.depth == 0)
            && self.static_precompiles.contains(&message.call_target);
        let result = if self.aa {
            let mut trace_message = message.clone();
            trace_message.depth += 1;
            self.inner.call(interp, &mut trace_message)
        } else {
            self.inner.call(interp, message)
        };
        self.inner.config_mut().exclude_precompile_calls = exclude_precompiles;
        result
    }

    fn call_end(
        &mut self,
        interp: &mut Interpreter<'_, '_, T>,
        message: &Message<T>,
        result: &mut MessageResult<T>,
    ) {
        self.inner.call_end(interp, message, result);
        self.active_frames -= 1;
    }

    fn create(
        &mut self,
        interp: &mut Interpreter<'_, '_, T>,
        message: &mut Message<T>,
    ) -> Option<MessageResult<T>> {
        self.enter(interp);
        self.active_frames += 1;
        if self.aa {
            let mut trace_message = message.clone();
            trace_message.depth += 1;
            self.inner.create(interp, &mut trace_message)
        } else {
            self.inner.create(interp, message)
        }
    }

    fn create_end(
        &mut self,
        interp: &mut Interpreter<'_, '_, T>,
        message: &Message<T>,
        result: &mut MessageResult<T>,
    ) {
        self.inner.create_end(interp, message, result);
        self.active_frames -= 1;
    }

    fn selfdestruct(
        &mut self,
        contract: &Address,
        target: &Address,
        value: &U256,
        host: &mut Evm<'_, T>,
    ) {
        <TracingInspector as Inspector<T>>::selfdestruct(
            &mut self.inner,
            contract,
            target,
            value,
            host,
        );
    }
}
