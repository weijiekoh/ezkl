use std::env;
use std::fs::File;
use std::io::Write;
use halo2_proofs::poly::kzg::commitment::ParamsKZG;
use halo2curves::bn256::Bn256;
use halo2_proofs::poly::commitment::Params;
#[cfg(feature = "evm")]
use ezkl::pfsys::evm::aggregation::gen_srs;

pub fn main() {
    let args: Vec<String> = env::args().collect();
    assert!(args.len() > 2);

    // TODO: use Clap?
    // TODO: implement error handling
    let log_2_size = &args[args.len() - 2];
    let output_filename = &args[args.len() - 1];

    let log_2_size: u32 = log_2_size.parse().unwrap();
    let params: ParamsKZG<Bn256> = gen_srs(log_2_size);

    println!("Writing SRS to {}", output_filename);
    let mut file = File::create(output_filename).unwrap();
    let _ = params.write(&mut file);
    file.flush().unwrap();
}
