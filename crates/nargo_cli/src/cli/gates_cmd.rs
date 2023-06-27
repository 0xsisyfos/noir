use std::collections::HashMap;

use super::{NargoConfig, backend_vendor_cmd::{BackendCommand, ProofArtifact}};
use crate::{
    constants::{self},
    errors::CliError, cli::backend_vendor_cmd::execute_backend_cmd,
};

use acvm::Backend;
use clap::Args;
use nameof::name_of;
use tracing::debug;

use super::backend_vendor_cmd;

/// Given a proof and a program, verify whether the proof is valid
#[derive(Debug, Clone, Args)]
pub(crate) struct GatesCommand {
    #[clap(flatten)]
    pub(crate) proof_options: ProofArtifact,

    #[clap(flatten)]
    backend_options: BackendCommand
}

pub(crate) fn run<B: Backend>(
    _backend: &B,
    mut args: GatesCommand,
    config: NargoConfig,
) -> Result<(), CliError<B>> {    

    backend_vendor_cmd::configure_proof_artifact(&config, &mut args.proof_options);

    debug!("Supplied arguments: {:?}", args);

    let backend_executable_path = backend_vendor_cmd::resolve_backend(&args.backend_options)?;
    let mut raw_pass_through= args.backend_options.backend_arguments.unwrap_or_default();
    let mut backend_args = vec![String::from(constants::GATES_SUB_CMD)];
    backend_args.append(&mut raw_pass_through);

    let mut envs = HashMap::new();
    envs.insert(name_of!(nargo_artifact_path in NargoConfig).to_uppercase(), String::from(config.nargo_artifact_path.unwrap().as_os_str().to_str().unwrap()));
    
    execute_backend_cmd(&backend_executable_path, backend_args, &config.nargo_package_root, Some(envs)).map_err(|e| { CliError::BackendVendorError(e)})
}

