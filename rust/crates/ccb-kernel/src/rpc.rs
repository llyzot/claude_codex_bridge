//! Protocol shared by stdio and network serving. Commit failures are fatal.
use crate::{Kernel, API_VERSION};
use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};
#[derive(Deserialize)]
pub struct Request {
    pub api_version: u32,
    pub id: u64,
    pub op: String,
    #[serde(default)]
    pub args: Value,
}
pub fn execute(kernel: &mut Kernel, line: &str) -> Result<Value> {
    Ok(match serde_json::from_str::<Request>(line) {
        Ok(request) if request.api_version == API_VERSION => {
            match kernel.dispatch(&request.op, &request.args) {
                Ok(result) => {
                    kernel.store.commit()?;
                    json!({"api_version":API_VERSION,"id":request.id,"ok":true,"result":result})
                }
                Err(error) => {
                    kernel.store.rollback();
                    json!({"api_version":API_VERSION,"id":request.id,"ok":false,"error":error.to_string()})
                }
            }
        }
        Ok(request) => {
            json!({"api_version":API_VERSION,"id":request.id,"ok":false,"error":"unsupported api_version"})
        }
        Err(error) => {
            json!({"api_version":API_VERSION,"id":null,"ok":false,"error":error.to_string()})
        }
    })
}
