use std::sync::Arc;

use anyhow::Result;

use crate::app::dispatcher::Dispatcher;
use crate::app::nat_manager::NatManager;
use crate::config::Inbound;
use crate::proxy::tun;
use crate::Runner;
use crate::RuntimeId;

pub struct TunInboundListener {
    pub(crate) rt_id: RuntimeId,
    pub(crate) inbound: Inbound,
    pub(crate) dispatcher: Arc<Dispatcher>,
    pub(crate) nat_manager: Arc<NatManager>,
}

impl TunInboundListener {
    pub fn listen(&self) -> Result<Runner> {
        tun::inbound::new(
            self.rt_id,
            self.inbound.clone(),
            self.dispatcher.clone(),
            self.nat_manager.clone(),
        )
    }
}
