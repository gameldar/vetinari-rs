//! Dev workflows: the services that run vetinari's steps on the autognomus
//! engine. They decide what is due from the clock; the daemon runtime
//! decides when to ask.

pub mod api;
pub mod launcher;
pub mod lease;
pub mod mirror;
pub mod ports;
pub mod start;
pub mod worker;
