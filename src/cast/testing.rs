//! A scripted [`CastApi`] for UI and agent tests (CAST-8, CAST-9, CAST-10) that don't need the
//! real service: set the devices, the firewall detection and the job states, then read back
//! what the code under test asked for. Not for production use. OWNER: CAST-0, frozen: several
//! items test with it in parallel, so none edits it. A test that needs another answer wraps a
//! `FakeCast` in its own `CastApi` impl (every method has a default) in its own test file.

use std::net::Ipv4Addr;
use std::sync::Mutex;

use super::{CastApi, CastDiagnose, CastErrorKind, Device, DeviceId, FirewallJob, PairJob, PairState, PassOutcome, TestState, firewall};
use crate::player::PlayerError;
use crate::player::remote::RemoteOutput;

/// What a [`FakeCast`] was asked to do, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeCall {
    Browse,
    AddByAddress(Ipv4Addr),
    Forget(DeviceId),
    PairStart(DeviceId),
    /// The PIN's digits are not recorded.
    PairSubmit(PairJob),
    PairNewCode(PairJob),
    PairCancel(PairJob),
    FirewallCheck,
    FirewallAllow,
    FirewallRemove,
    FirewallCancel,
    StartPass,
    TestOnTv(DeviceId),
    ConvertCurrent,
    Open(DeviceId),
}

/// What [`FakeCast::open`] answers: a test keeps the other ends of the channels it hands out.
pub type OpenFn = Box<dyn Fn(DeviceId) -> Result<RemoteOutput, PlayerError> + Send + Sync>;

/// The scripted answers (public: tests set them through [`FakeCast::script`]).
#[derive(Default)]
pub struct FakeScript {
    pub devices: Vec<Device>,
    pub searching: bool,
    pub add_outcome: Option<Result<DeviceId, CastErrorKind>>,
    pub pair_state: Option<PairState>,
    pub firewall: Option<firewall::Detection>,
    pub firewall_job: FirewallJob,
    pub pass_outcome: Option<PassOutcome>,
    pub test_state: TestState,
    pub last_error: Option<CastErrorKind>,
    pub diagnose: Option<CastDiagnose>,
    /// What `open` answers (`None`: the inert error).
    pub open: Option<OpenFn>,
}

impl std::fmt::Debug for FakeScript {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeScript").field("devices", &self.devices).field("open", &self.open.is_some()).finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
pub struct FakeCast {
    script: Mutex<FakeScript>,
    calls: Mutex<Vec<FakeCall>>,
}

impl FakeCast {
    /// Change the scripted answers.
    pub fn script(&self, f: impl FnOnce(&mut FakeScript)) {
        f(&mut self.script.lock().unwrap_or_else(|e| e.into_inner()));
    }

    /// Everything asked so far (and clear the record).
    pub fn take_calls(&self) -> Vec<FakeCall> {
        std::mem::take(&mut self.calls.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn call(&self, c: FakeCall) {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).push(c);
    }

    fn read<T>(&self, f: impl FnOnce(&mut FakeScript) -> T) -> T {
        f(&mut self.script.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

impl CastApi for FakeCast {
    fn devices(&self) -> Vec<Device> {
        self.read(|s| s.devices.clone())
    }
    fn browse(&self) {
        self.call(FakeCall::Browse);
    }
    fn searching(&self) -> bool {
        self.read(|s| s.searching)
    }
    fn add_by_address(&self, ip: Ipv4Addr) {
        self.call(FakeCall::AddByAddress(ip));
    }
    fn take_add_outcome(&self) -> Option<Result<DeviceId, CastErrorKind>> {
        self.read(|s| s.add_outcome.take())
    }
    fn forget(&self, device: DeviceId) {
        self.call(FakeCall::Forget(device));
    }
    fn pair_start(&self, device: DeviceId) -> PairJob {
        self.call(FakeCall::PairStart(device));
        PairJob(1)
    }
    fn pair_state(&self, _job: PairJob) -> PairState {
        self.read(|s| s.pair_state.unwrap_or(PairState::Gone))
    }
    fn pair_submit(&self, job: PairJob, _pin: super::Pin) {
        self.call(FakeCall::PairSubmit(job));
    }
    fn pair_new_code(&self, job: PairJob) {
        self.call(FakeCall::PairNewCode(job));
    }
    fn pair_cancel(&self, job: PairJob) {
        self.call(FakeCall::PairCancel(job));
    }
    fn firewall(&self) -> Option<firewall::Detection> {
        self.read(|s| s.firewall.clone())
    }
    fn firewall_check(&self) {
        self.call(FakeCall::FirewallCheck);
    }
    fn firewall_allow(&self) {
        self.call(FakeCall::FirewallAllow);
    }
    fn firewall_remove(&self) {
        self.call(FakeCall::FirewallRemove);
    }
    fn firewall_cancel(&self) {
        self.call(FakeCall::FirewallCancel);
    }
    fn firewall_job(&self) -> FirewallJob {
        self.read(|s| s.firewall_job.clone())
    }
    fn start_pass(&self) {
        self.call(FakeCall::StartPass);
    }
    fn take_pass_outcome(&self) -> Option<PassOutcome> {
        self.read(|s| s.pass_outcome.take())
    }
    fn test_on_tv(&self, device: DeviceId) {
        self.call(FakeCall::TestOnTv(device));
    }
    fn test_state(&self) -> TestState {
        self.read(|s| s.test_state)
    }
    fn convert_current(&self) {
        self.call(FakeCall::ConvertCurrent);
    }
    fn last_error(&self) -> Option<CastErrorKind> {
        self.read(|s| s.last_error)
    }
    fn diagnose(&self) -> CastDiagnose {
        self.read(|s| s.diagnose.clone()).unwrap_or_else(|| CastDiagnose { timing: "ptp-declared", ..CastDiagnose::default() })
    }
    fn open(&self, device: DeviceId) -> Result<RemoteOutput, PlayerError> {
        self.call(FakeCall::Open(device));
        let script = self.script.lock().unwrap_or_else(|e| e.into_inner());
        match &script.open {
            Some(f) => f(device),
            None => super::NullCast.open(device),
        }
    }
}
