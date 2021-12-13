use three_em_arweave::arweave::Arweave;
use three_em_executor::execute_contract;

#[tokio::main]
async fn main() {
  let arweave = Arweave::new(443, "arweave.net".to_string());

  execute_contract(
    arweave,
    "_233QEbUxpTpxa_CUbGi3TVEEh2Qao5i_xzp4Lusv8I".to_string(),
    None,
    None,
    None,
    true,
  )
  .await
  .unwrap();
}
