use std::{
    fs,
    io::Cursor,
    process::exit,
    sync::{Arc, Mutex},
};

use arith::{Field, FieldSerde, FieldSerdeError};
use circuit::Circuit;
use config::{
    BN254ConfigMIMC5, Config, FieldType, GF2ExtConfigSha2, GKRConfig, GKRScheme, M31ExtConfigSha2,
    MPIConfig, SENTINEL_BN254, SENTINEL_GF2, SENTINEL_M31,
};
use log::{debug, info};
use transcript::Proof;
use warp::{http::StatusCode, reply, Filter};

fn dump_proof_and_claimed_v<F: Field + FieldSerde>(
    proof: &Proof,
    claimed_v: &F,
) -> Result<Vec<u8>, FieldSerdeError> {
    let mut bytes = Vec::new();

    proof.serialize_into(&mut bytes)?;
    claimed_v.serialize_into(&mut bytes)?;

    Ok(bytes)
}

fn load_proof_and_claimed_v<F: Field + FieldSerde>(
    bytes: &[u8],
) -> Result<(Proof, F), FieldSerdeError> {
    let mut cursor = Cursor::new(bytes);

    let proof = Proof::deserialize_from(&mut cursor)?;
    let claimed_v = F::deserialize_from(&mut cursor)?;

    Ok((proof, claimed_v))
}

fn detect_field_type_from_circuit_file(circuit_file: &str) -> FieldType {
    // read last 32 byte of sentinel field element to determine field type
    let bytes = fs::read(circuit_file).expect("Unable to read circuit file.");
    let field_bytes = &bytes[8..8 + 32];
    match field_bytes.try_into().unwrap() {
        SENTINEL_M31 => FieldType::M31,
        SENTINEL_BN254 => FieldType::BN254,
        SENTINEL_GF2 => FieldType::GF2,
        _ => {
            println!("Unknown field type. Field byte value: {:?}", field_bytes);
            exit(1);
        }
    }
}

async fn run_command<'a, C: GKRConfig>(
    command: &str,
    circuit_file: &str,
    config: Config<C>,
    args: &[String],
) {
    match command {
        "prove" => {
            let witness_file = &args[3];
            let output_file = &args[4];
            let mut circuit = Circuit::<C>::load_circuit(circuit_file);
            circuit.load_witness_file(witness_file);
            let mut prover = gkr::Prover::new(&config);
            prover.prepare_mem(&circuit);
            let (claimed_v, proof) = prover.prove(&mut circuit);

            if config.mpi_config.is_root() {
                let bytes = dump_proof_and_claimed_v(&proof, &claimed_v)
                    .expect("Unable to serialize proof.");
                fs::write(output_file, bytes).expect("Unable to write proof to file.");
            }
        }
        "verify" => {
            let witness_file = &args[3];
            let output_file = &args[4];
            let mut circuit = Circuit::<C>::load_circuit(circuit_file);
            circuit.load_witness_file(witness_file);

            // Repeating the same public input for mpi_size times
            // TODO: Fix this, use real input
            if args.len() > 5 {
                let mpi_size = args[5].parse::<i32>().unwrap();
                let n_public_input_per_mpi = circuit.public_input.len();
                for _ in 1..mpi_size {
                    circuit
                        .public_input
                        .append(&mut circuit.public_input[..n_public_input_per_mpi].to_owned());
                }
            }
            let bytes = fs::read(output_file).expect("Unable to read proof from file.");
            let (proof, claimed_v) =
                load_proof_and_claimed_v(&bytes).expect("Unable to deserialize proof.");
            let verifier = gkr::Verifier::new(&config);
            let public_input = circuit.public_input.clone();
            assert!(verifier.verify(&mut circuit, &public_input, &claimed_v, &proof));
            println!("success");
        }
        "serve" => {
            let host: [u8; 4] = args[3]
                .split('.')
                .map(|s| s.parse().unwrap())
                .collect::<Vec<u8>>()
                .try_into()
                .unwrap();
            let port = args[4].parse().unwrap();
            let circuit = Circuit::<C>::load_circuit(circuit_file);
            let mut prover = gkr::Prover::new(&config);
            prover.prepare_mem(&circuit);
            let verifier = gkr::Verifier::new(&config);
            let circuit = Arc::new(Mutex::new(circuit));
            let circuit_clone_for_verifier = circuit.clone();
            let prover = Arc::new(Mutex::new(prover));
            let verifier = Arc::new(Mutex::new(verifier));
            let ready_time = chrono::offset::Utc::now();
            let ready = warp::path("ready").map(move || {
                info!("Received ready request.");
                reply::with_status(format!("Ready since {:?}", ready_time), StatusCode::OK)
            });
            let prove =
                warp::path("prove")
                    .and(warp::body::bytes())
                    .map(move |bytes: bytes::Bytes| {
                        info!("Received prove request.");
                        let witness_bytes: Vec<u8> = bytes.to_vec();
                        let mut circuit = circuit.lock().unwrap();
                        let mut prover = prover.lock().unwrap();
                        circuit.load_witness_bytes(&witness_bytes, true);
                        let (claimed_v, proof) = prover.prove(&mut circuit);
                        reply::with_status(
                            dump_proof_and_claimed_v(&proof, &claimed_v).unwrap(),
                            StatusCode::OK,
                        )
                    });
            let verify =
                warp::path("verify")
                    .and(warp::body::bytes())
                    .map(move |bytes: bytes::Bytes| {
                        info!("Received verify request.");
                        let witness_and_proof_bytes: Vec<u8> = bytes.to_vec();
                        let length_of_witness_bytes =
                            u64::from_le_bytes(witness_and_proof_bytes[0..8].try_into().unwrap())
                                as usize;
                        let length_of_proof_bytes =
                            u64::from_le_bytes(witness_and_proof_bytes[8..16].try_into().unwrap())
                                as usize;
                        let witness_bytes =
                            &witness_and_proof_bytes[16..16 + length_of_witness_bytes];
                        let proof_bytes = &witness_and_proof_bytes[16 + length_of_witness_bytes
                            ..16 + length_of_witness_bytes + length_of_proof_bytes];

                        let mut circuit = circuit_clone_for_verifier.lock().unwrap();
                        let verifier = verifier.lock().unwrap();
                        circuit.load_witness_bytes(witness_bytes, true);
                        let public_input = circuit.public_input.clone();
                        let (proof, claimed_v) = load_proof_and_claimed_v(proof_bytes).unwrap();
                        if verifier.verify(&mut circuit, &public_input, &claimed_v, &proof) {
                            "success".to_string()
                        } else {
                            "failure".to_string()
                        }
                    });
            warp::serve(
                warp::post()
                    .and(prove.or(verify))
                    .or(warp::get().and(ready)),
            )
            .run((host, port))
            .await;
        }
        _ => {
            println!("Invalid command.");
        }
    }
}

#[tokio::main]
async fn main() {
    // examples:
    // expander-exec prove <input:circuit_file> <input:witness_file> <output:proof>
    // expander-exec verify <input:circuit_file> <input:witness_file> <input:proof> <input:mpi_size>
    // expander-exec serve <input:circuit_file> <input:ip> <input:port>
    let mut mpi_config = MPIConfig::new();

    let args = std::env::args().collect::<Vec<String>>();
    if args.len() < 5 {
        println!(
            "Usage: expander-exec prove <input:circuit_file> <input:witness_file> <output:proof>"
        );
        println!(
            "Usage: expander-exec verify <input:circuit_file> <input:witness_file> <input:proof> <input:mpi_size>"
        );
        println!("Usage: expander-exec serve <input:circuit_file> <input:host> <input:port>");
        return;
    }
    let command = &args[1];
    if command != "prove" && command != "verify" && command != "serve" {
        println!("Invalid command.");
        return;
    }

    if command == "verify" && args.len() > 5 {
        assert!(mpi_config.world_size == 1); // verifier should not be run with mpiexec
        mpi_config.world_size = args[5].parse::<i32>().expect("Parsing mpi size fails");
    }

    let circuit_file = &args[2];
    let field_type = detect_field_type_from_circuit_file(circuit_file);
    debug!("field type: {:?}", field_type);
    match field_type {
        FieldType::M31 => {
            run_command::<M31ExtConfigSha2>(
                command,
                circuit_file,
                Config::<M31ExtConfigSha2>::new(GKRScheme::Vanilla, mpi_config.clone()),
                &args,
            )
            .await;
        }
        FieldType::BN254 => {
            run_command::<BN254ConfigMIMC5>(
                command,
                circuit_file,
                Config::<BN254ConfigMIMC5>::new(GKRScheme::Vanilla, mpi_config.clone()),
                &args,
            )
            .await;
        }
        FieldType::GF2 => {
            run_command::<GF2ExtConfigSha2>(
                command,
                circuit_file,
                Config::<GF2ExtConfigSha2>::new(GKRScheme::Vanilla, mpi_config.clone()),
                &args,
            )
            .await
        }
    }

    MPIConfig::finalize();
}
