use rpc::jsonrpc_core::*;
use rpc::Web3;

pub struct Web3Client;

impl Web3Client {
	pub fn new() -> Self { Web3Client }
}

impl Web3 for Web3Client {
	fn client_version(&self, params: Params) -> Result<Value, Error> {
		match params {
			Params::None => Ok(Value::String("parity/0.1.0/-/rust1.7-nightly".to_string())),
			_ => Err(Error::invalid_params())
		}
	}
}
