//! Dev workflows: the services that run vetinari's steps on the autognomus
//! engine. They decide what is due from the clock; the daemon runtime
//! decides when to ask.

pub mod lease;
pub mod ports;
