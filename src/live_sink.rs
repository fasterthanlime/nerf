use nwind::UserFrame;

pub struct SampleEvent< 'a > {
    pub timestamp: u64,
    pub pid: u32,
    pub tid: u32,
    pub cpu: u32,
    pub kernel_backtrace: &'a [u64],
    pub user_backtrace: &'a [UserFrame],
}

pub trait LiveSink: Send + Sync {
    fn on_sample( &self, event: &SampleEvent );
}
