use std::collections::HashMap;

use crate::structs::ServerRuntime;

pub type ListenAddr = String;
pub type ServersByListen = HashMap<ListenAddr, Vec<ServerRuntime>>;
