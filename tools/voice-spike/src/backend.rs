#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BackendDiagnostics {
    pub nan_recoveries: u64,
}

pub trait Backend {
    fn name(&self) -> &'static str;
    fn process(&mut self, input: &[f32], output: &mut [f32]) -> usize;
    fn latency_frames(&self) -> usize;
    fn reset(&mut self);
    fn diagnostics(&self) -> BackendDiagnostics {
        BackendDiagnostics::default()
    }
}

pub struct PassthroughBackend;

impl Backend for PassthroughBackend {
    fn name(&self) -> &'static str {
        "passthrough"
    }

    fn process(&mut self, input: &[f32], output: &mut [f32]) -> usize {
        let n = input.len().min(output.len());
        output[..n].copy_from_slice(&input[..n]);
        n
    }

    fn latency_frames(&self) -> usize {
        0
    }

    fn reset(&mut self) {}
}

pub struct DelayBackend {
    delay: usize,
    history: Vec<f32>,
    pos: usize,
}

impl DelayBackend {
    pub fn new(delay: usize) -> Self {
        Self {
            delay,
            history: vec![0.0; delay.max(1)],
            pos: 0,
        }
    }
}

impl Backend for DelayBackend {
    fn name(&self) -> &'static str {
        "delay"
    }

    fn process(&mut self, input: &[f32], output: &mut [f32]) -> usize {
        let n = input.len().min(output.len());
        for i in 0..n {
            let delayed = self.history[self.pos];
            self.history[self.pos] = input[i];
            self.pos = (self.pos + 1) % self.history.len();
            output[i] = delayed;
        }
        n
    }

    fn latency_frames(&self) -> usize {
        self.delay
    }

    fn reset(&mut self) {
        self.history.fill(0.0);
        self.pos = 0;
    }
}
