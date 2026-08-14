pub mod v1 {
    tonic::include_proto!("proxy_tester.v1");
}

mod conversions;

pub use conversions::{ConversionError, network_draft_from_wire, network_draft_to_wire};
