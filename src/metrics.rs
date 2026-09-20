#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionMetrics {
    pub physical_loc: u32,
    pub source_loc: u32,
    pub cyclomatic_complexity: u32,
    pub cognitive_complexity: u32,
}
