//! Extraction eval suite (Plan 05b): synthetic corpus + deterministic grader +
//! gated real-API runner. Foundation for a prompt-optimization loop — scores are
//! comparable across prompt variants. Pure consumer of `murmur-core` public API;
//! zero impact on shipping code.

pub mod corpus;
pub mod grade;
pub mod normalize;
