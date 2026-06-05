/// 占位:证明工程能编译能测。后续任务会替换/扩充本文件。
pub fn spike_ready() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_builds_and_runs() {
        assert!(spike_ready());
    }
}
