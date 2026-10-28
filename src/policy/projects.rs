use crate::plan::CandidateKind;
use std::ffi::OsStr;
pub(crate) fn classify(name: &OsStr) -> Option<CandidateKind> {
    match name.to_str()? {
        "node_modules" => Some(CandidateKind::NodeDependencies),
        "__pycache__" => Some(CandidateKind::PythonBytecode),
        ".venv" | ".venvs" | "venv" => Some(CandidateKind::PythonEnvironment),
        _ => None,
    }
}
