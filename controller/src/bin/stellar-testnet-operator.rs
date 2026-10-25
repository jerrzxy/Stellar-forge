// Copyright 2024 Stellar-K8s Contributors
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

// Copyright 2024 Stellar-K8s Contributors
// SPDX-License-Identifier: Apache-2.0
//! Stellar-K8s controller: provisions ephemeral Stellar testnets from
//! `StellarTestnet` custom resources.
//!
//! Usage:
//!   stellar-testnet-operator          run the operator against the current
//!                                     cluster (kubeconfig or in-cluster)
//!   stellar-testnet-operator crd      print the `StellarTestnet` CRD YAML

use std::sync::Arc;

use anyhow::Context as _;
use futures::StreamExt;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::Service;
use k8s_openapi::api::networking::v1::Ingress;
use kube::{
    api::{Patch, PatchParams},
    runtime::{controller::Controller, watcher},
    Api, Client, CustomResourceExt,
};
use tracing::info;
use tracing_subscriber::EnvFilter;

use controller::crd::testnet::{StellarTestnet, GROUP, PLURAL, VERSION};
use controller::testnet::{client, provisioner};

/// Field manager used when installing the CRD at startup.
const CRD_MANAGER: &str = "stellar-testnet-operator-crd";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    if std::env::args().any(|arg| arg == "crd") {
        print!("{}", serde_yaml::to_string(&StellarTestnet::crd())?);
        return Ok(());
    }

    let client = Client::try_default().await.context(
        "kubernetes client (is the controller running in-cluster or with a kubeconfig?)",
    )?;
    install_crd(&client).await?;

    let stellartestnets: Api<StellarTestnet> = Api::all(client.clone());
    let ctx = Arc::new(provisioner::Context {
        client: client.clone(),
        http: client::default_http_client(),
    });

    info!(
        agent = provisioner::AGENT,
        plural = PLURAL,
        version = VERSION,
        "starting StellarTestnet controller"
    );

    Controller::new(stellartestnets, watcher::Config::default().any_semantic())
        .watches(
            Api::<Deployment>::all(client.clone()),
            watcher::Config::default().any_semantic(),
            provisioner::owned_by_stellartestnet,
        )
        .watches(
            Api::<Service>::all(client.clone()),
            watcher::Config::default().any_semantic(),
            provisioner::owned_by_stellartestnet,
        )
        .watches(
            Api::<Ingress>::all(client.clone()),
            watcher::Config::default().any_semantic(),
            provisioner::owned_by_stellartestnet,
        )
        .shutdown_on_signal()
        .run(provisioner::reconcile, provisioner::error_policy, ctx)
        .for_each(|result| async move {
            match result {
                Ok((object, action)) => {
                    info!(testnet = %object, ?action, "reconcile finished")
                }
                Err(error) => tracing::error!(error = %error, "reconcile error"),
            }
        })
        .await;

    info!("controller stopped");
    Ok(())
}

/// Install the `StellarTestnet` CRD if it is missing so a fresh operator
/// can serve the resource immediately.
async fn install_crd(client: &Client) -> anyhow::Result<()> {
    use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;

    let crds: Api<CustomResourceDefinition> = Api::all(client.clone());
    let name = format!("{PLURAL}.{}", GROUP);
    match crds.get(&name).await {
        Ok(_) => {
            info!(crd = %name, "CRD already installed");
        }
        Err(kube::Error::Api(error)) if error.code == 404 => {
            info!(crd = %name, "installing CRD");
            crds.patch(
                &name,
                &PatchParams::apply(CRD_MANAGER).force(),
                &Patch::Apply(&StellarTestnet::crd()),
            )
            .await
            .context("installing StellarTestnet CRD")?;
        }
        Err(error) => return Err(error).context("checking for StellarTestnet CRD"),
    }
    Ok(())
}
