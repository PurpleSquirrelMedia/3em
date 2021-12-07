use deno_core::error::AnyError;
use deno_core::serde_json;
use deno_core::serde_json::Value;
use serde_json::value::Value::Null;
use std::collections::HashMap;
use std::time::Instant;
use three_em_arweave::arweave::{Arweave, ARWEAVE_CACHE};
use three_em_arweave::gql_result::{
  GQLEdgeInterface, GQLNodeInterface, GQLTagInterface,
};
use three_em_arweave::miscellaneous::get_sort_key;
use three_em_arweave::miscellaneous::ContractType;
use three_em_js::Runtime;
use three_em_smartweave::ContractBlock;
use three_em_smartweave::ContractInfo;
use three_em_wasm::WasmRuntime;

struct ContractHandlerResult {
  result: Option<Value>,
  state: Option<Value>,
}

pub type ValidityTable = HashMap<String, bool>;

pub enum ExecuteResult {
  V8(Value, ValidityTable),
  Evm(Vec<u8>, ValidityTable),
}

pub async fn execute_contract(
  arweave: Arweave,
  contract_id: String,
  contract_src_tx: Option<String>,
  contract_content_type: Option<String>,
  height: Option<usize>,
  cache: bool,
) -> ExecuteResult {
  let contract_id_copy = contract_id.to_owned();
  let shared_id = contract_id.clone();
  let shared_client = arweave.clone();

  let (loaded_contract, interactions) = tokio::join!(
    tokio::spawn(async move {
      let mut contract = shared_client
        .load_contract(shared_id, contract_src_tx, contract_content_type, cache)
        .await;

      contract
    }),
    tokio::spawn(async move {
      let (
        result_interactions,
        new_interaction_index,
        are_there_new_interactions,
      ) = arweave.get_interactions(contract_id, height, cache).await;

      let mut interactions = result_interactions;

      interactions.sort_by(|a, b| {
        let a_sort_key =
          get_sort_key(&a.node.block.height, &a.node.block.id, &a.node.id);
        let b_sort_key =
          get_sort_key(&b.node.block.height, &b.node.block.id, &b.node.id);
        a_sort_key.cmp(&b_sort_key)
      });

      (
        interactions,
        new_interaction_index,
        are_there_new_interactions,
      )
    })
  );

  let loaded_contract = loaded_contract.unwrap();
  let (result_interactions, new_interaction_index, are_there_new_interactions) =
    interactions.unwrap();

  let mut interactions = result_interactions;

  let mut validity: HashMap<String, bool> = HashMap::new();
  let transaction = loaded_contract.contract_transaction;
  let contract_info = ContractInfo {
    transaction,
    block: ContractBlock {
      height: 0,
      indep_hash: String::from(""),
      timestamp: String::from(""),
    },
  };

  let mut needs_processing = true;
  let mut cache_state: Option<Value> = None;

  if cache {
    let get_cached_state =
      ARWEAVE_CACHE.find_state(contract_id_copy.to_owned()).await;

    if let Some(cached_state) = get_cached_state {
      cache_state = Some(cached_state.state);
      validity = cached_state.validity;
      needs_processing = are_there_new_interactions;
    }
  }

  let is_cache_state_present = cache_state.is_some();

  // TODO: handle evm.
  match loaded_contract.contract_type {
    ContractType::JAVASCRIPT => {
      if needs_processing {
        let mut state: Value = cache_state.unwrap_or_else(|| {
          deno_core::serde_json::from_str(&loaded_contract.init_state).unwrap()
        });

        let mut rt = Runtime::new(
          &(String::from_utf8(loaded_contract.contract_src).unwrap()),
          state,
          contract_info,
        )
        .await
        .unwrap();

        if cache && is_cache_state_present && are_there_new_interactions {
          interactions = (&interactions[new_interaction_index..]).to_vec();
        }

        for interaction in interactions {
          let tx = interaction.node;
          let input = get_input_from_interaction(&tx);

          // TODO: has_multiple_interactions
          // https://github.com/ArweaveTeam/SmartWeave/blob/4d09c66d832091805f583ba73e8da96cde2c0190/src/contract-read.ts#L68
          let js_input: Value = deno_core::serde_json::from_str(input).unwrap();

          let call_input = serde_json::json!({
            "input": js_input,
            "caller": tx.owner.address
          });

          let valid = rt.call(call_input).await.is_ok();
          validity.insert(tx.id, valid);
        }

        let state_val: Value = rt.get_contract_state().unwrap();

        if cache {
          ARWEAVE_CACHE
            .cache_states(contract_id_copy.to_owned(), &state_val, &validity)
            .await;
        }

        ExecuteResult::V8(state_val, validity)
      } else {
        ExecuteResult::V8(cache_state.unwrap(), validity)
      }
    }
    ContractType::WASM => {
      if needs_processing {
        let wasm = loaded_contract.contract_src.as_slice();

        let init_state_wasm = if cache_state.is_some() {
          let cache_state_unwrapped = cache_state.unwrap();
          let state_str = cache_state_unwrapped.to_string();
          state_str.as_bytes().to_vec()
        } else {
          loaded_contract.init_state.as_bytes().to_vec()
        };

        let mut state = init_state_wasm;
        let mut rt = WasmRuntime::new(wasm, contract_info).unwrap();

        if cache && is_cache_state_present && are_there_new_interactions {
          interactions = (&interactions[new_interaction_index..]).to_vec();
        }

        for interaction in interactions {
          let tx = interaction.node;
          let input = get_input_from_interaction(&tx);
          let wasm_input: Value =
            deno_core::serde_json::from_str(input).unwrap();
          let call_input = serde_json::json!({
            "input": wasm_input,
            "caller": tx.owner.address,
          });

          let mut input = deno_core::serde_json::to_vec(&call_input).unwrap();
          let exec = rt.call(&mut state, &mut input);
          let valid = exec.is_ok();
          if valid {
            state = exec.unwrap();
          }
          validity.insert(tx.id, valid);
        }

        let state: Value = deno_core::serde_json::from_slice(&state).unwrap();

        if cache {
          ARWEAVE_CACHE
            .cache_states(contract_id_copy.to_owned(), &state, &validity)
            .await;
        }

        ExecuteResult::V8(state, validity)
      } else {
        ExecuteResult::V8(cache_state.unwrap(), validity)
      }
    }
    ContractType::EVM => ExecuteResult::V8(Null, validity),
  }
}

pub fn get_input_from_interaction(interaction_tx: &GQLNodeInterface) -> &str {
  let tag = &(&interaction_tx)
    .tags
    .iter()
    .find(|data| &data.name == "Input");

  match tag {
    Some(data) => &data.value,
    None => "",
  }
}

pub fn has_multiple_interactions(interaction_tx: &GQLNodeInterface) -> bool {
  let tags = (&interaction_tx.tags).to_owned();
  let filtered_tags = tags
    .iter()
    .filter(|data| data.name == String::from("Contract"))
    .cloned()
    .collect::<Vec<GQLTagInterface>>();
  filtered_tags.len() > 1
}

#[cfg(test)]
mod test {
  use crate::execute_contract;
  use crate::ExecuteResult;
  use deno_core::serde_json;
  use serde::Deserialize;
  use serde::Serialize;
  use three_em_arweave::arweave::Arweave;

  #[derive(Deserialize, Serialize)]
  struct People {
    username: String,
  }

  #[tokio::test]
  async fn test_execute_wasm() {
    let arweave = Arweave::new(80, String::from("arweave.net"));
    let result = execute_contract(
      arweave,
      String::from("KfU_1Uxe3-h2r3tP6ZMfMT-HBFlM887tTFtS-p4edYQ"),
      None,
      None,
      Some(822062),
      false,
    )
    .await;
    if let ExecuteResult::V8(value, validity) = result {
      assert!(!(value.is_null()));
      assert!(value.get("counter").is_some());
      let counter = value.get("counter").unwrap().as_i64().unwrap();
      assert_eq!(counter, 2);
      assert!(validity
        .get("HBHsDDeWrEmAlkg_mFzYjOsEgG3I6j4id_Aqd1fERgA")
        .is_some());
      assert!(validity
        .get("IlAr0h0rl7oI7FesF1Oy-E_a-K6Al4Avc2pu6CEZkog")
        .is_some());
    } else {
      assert!(false);
    }
  }

  #[tokio::test]
  async fn test_execute_javascript() {
    let arweave = Arweave::new(80, String::from("arweave.net"));
    let result = execute_contract(
      arweave,
      String::from("t9T7DIOGxx4VWXoCEeYYarFYeERTpWIC1V3y-BPZgKE"),
      None,
      None,
      None,
      false,
    )
    .await;
    if let ExecuteResult::V8(value, validity) = result {
      assert!(!(value.is_null()));
      assert!(value.get("people").is_some());
      assert!(value.get("people").unwrap().is_array());
      let people = value.get("people").unwrap();
      let people_struct: Vec<People> =
        serde_json::from_value(people.to_owned()).unwrap();
      let is_marton_here = people_struct
        .iter()
        .find(|data| data.username == String::from("martonlederer"));
      assert!(is_marton_here.is_some());
    } else {
      assert!(false);
    }
  }
}
