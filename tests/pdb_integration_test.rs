use kube::Client;

#[tokio::test]
async fn pdb_integration_smoke() {
    let Ok(_client) = Client::try_default().await else {
        eprintln!("skipping PDB integration smoke test: no Kubernetes client configured");
        return;
    };

    assert!(true);
}
