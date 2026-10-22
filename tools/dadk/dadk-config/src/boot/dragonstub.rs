use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct DragonStubConfig {
    /// The path to the source code of the DragonStub project.
    #[serde(rename = "src-path")]
    pub src_path: String,
}
