//! Change simulation.
//!
//! The reasoner enables Copilot to tackle complex tasks by iterating through:
//! 1. Propose: generate hypothesis
//! 2. Critique: check against anti-patterns + graph conflicts
//! 3. Refine: update hypothesis
//! 4. Assess: score confidence; halt or continue
//!
//! Inspired by OpenMythos multi-step reasoning.

pub mod simulator;
