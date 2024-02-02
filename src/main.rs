use actix_files as fs;
use actix_web::{error, web, App, HttpResponse, HttpServer, Responder, Result};
use anyhow::Context as AnyhowContext;
use clap::Parser;
use near_account_id::AccountId;
use near_crypto::{InMemorySigner, PublicKey, Signer};
use near_jsonrpc_client::{methods, JsonRpcClient};
use near_jsonrpc_primitives::types::query::QueryResponseKind;
use near_primitives::action::{Action, AddKeyAction, CreateAccountAction, TransferAction};
use near_primitives::transaction::{SignedTransaction, Transaction};
use near_primitives::types::{BlockReference, Finality};
use near_primitives::views::FinalExecutionStatus;
use near_primitives_core::account::AccessKey;
use near_primitives_core::hash::CryptoHash;
use near_primitives_core::types::{Balance, Nonce};
use serde::Deserialize;
use std::str::FromStr;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tera::ast::In;
use tera::{Context, Tera};

/// Simple server configuration
#[derive(Parser)]
#[clap(author, version, about, long_about = None)]
struct Args {
    /// Port to listen on, default 8080
    #[clap(short, long, default_value_t = 8080)]
    port: u16,
    #[clap(long)]
    near_rpc_url: String,
    #[clap(long)]
    base_key_file: String,
}

/// Index page repsonding with just a template rendering
/// The template has a form for submission that should be handled by the method `create_account`
async fn index(tera: web::Data<Tera>) -> Result<impl Responder> {
    println!("index");
    let _context = Context::new();

    let rendered = tera.render("index.html.tera", &_context).map_err(|err| {
        error::ErrorInternalServerError(format!("Failed to render template: {:?}", err))
    })?;

    Ok(HttpResponse::Ok().content_type("text/html").body(rendered))
}

// TODO: rate limit or somehow gate this faucet

async fn send_create_account(
    near_rpc: &JsonRpcClient,
    base_signer: &InMemorySigner,
    account_id: &str,
    public_key: &str,
    nonce: Nonce,
    block_hash: CryptoHash,
    funding_amount: Balance,
) -> anyhow::Result<()> {
    let new_account = AccountId::from_str(account_id)
        .with_context(|| format!("failed parsing account ID: {}", account_id))?;
    let pkey = PublicKey::from_str(public_key)
        .with_context(|| format!("failed parsing public key: {}", public_key))?;

    let actions = vec![
        Action::CreateAccount(CreateAccountAction {}),
        Action::AddKey(Box::new(AddKeyAction {
            public_key: pkey,
            access_key: AccessKey::full_access(),
        })),
        Action::Transfer(TransferAction {
            deposit: funding_amount,
        }),
    ];

    let tx = Transaction {
        signer_id: base_signer.account_id.clone(),
        public_key: base_signer.public_key.clone(),
        nonce,
        receiver_id: new_account,
        block_hash,
        actions,
    };
    let (hash, _size) = tx.get_hash_and_size();
    let sig = base_signer.sign(hash.as_ref());
    let signed_transaction = SignedTransaction::new(sig, tx);

    // TODO: retry on nonce error
    match near_rpc
        .call(methods::broadcast_tx_commit::RpcBroadcastTxCommitRequest { signed_transaction })
        .await
    {
        Ok(r) => {
            if matches!(r.status, FinalExecutionStatus::SuccessValue(_)) {
                Ok(())
            } else {
                Err(anyhow::anyhow!(
                    "transaction execution failed: {:?}",
                    &r.status
                ))
            }
        }
        Err(e) => Err(e.into()),
    }
}

#[derive(Deserialize)]
pub struct FormData {
    account_id: String,
    public_key: String,
}

#[derive(Clone)]
struct NearData {
    base_signer: InMemorySigner,
    nonce: Arc<Mutex<Nonce>>,
    block_hash: Arc<RwLock<CryptoHash>>,
    rpc: JsonRpcClient,
}

async fn create_account(
    near: web::Data<NearData>,
    tera: web::Data<Tera>,
    form: web::Form<FormData>,
) -> Result<impl Responder> {
    let data = form.into_inner();

    let block_hash = *near.block_hash.read().unwrap();
    // for now we keep the lock while calling send_create_account(),
    // but TODO is to not do that and just retry if the nonce fails
    let mut nonce = near.nonce.lock().unwrap();
    *nonce += 1;

    match send_create_account(
        &near.rpc,
        &near.base_signer,
        &data.account_id,
        &data.public_key,
        *nonce,
        block_hash,
        100, // TODO: decide this
    )
    .await
    {
        Ok(_) => {
            eprintln!(
                "successfully created {} {}",
                &data.account_id, &data.public_key
            );

            let mut context = Context::new();
            context.insert("account_id", &data.account_id);
            context.insert("public_key", &data.public_key);

            match tera.render("form_success.html.tera", &context) {
                Ok(rendered) => Ok(HttpResponse::Ok().content_type("text/html").body(rendered)),
                Err(err) => Err(error::ErrorInternalServerError(format!(
                    "Failed to render template: {:?}",
                    err
                ))),
            }
        }
        Err(err) => {
            eprintln!("cant create account: {:?}", err);
            // TODO: send an http response that says it failed
            Err(error::ErrorServiceUnavailable("Failed to create account"))
        }
    }
}

async fn current_block_hash(near_rpc: &JsonRpcClient) -> std::io::Result<CryptoHash> {
    match near_rpc.call(methods::status::RpcStatusRequest).await {
        Ok(status) => Ok(status.sync_info.latest_block_hash),
        Err(e) => Err(std::io::Error::other(e)),
    }
}

async fn update_block_hash(near_rpc: JsonRpcClient, block_hash: Arc<RwLock<CryptoHash>>) {
    loop {
        actix_rt::time::sleep(Duration::from_secs(30)).await;

        let current = match current_block_hash(&near_rpc).await {
            Ok(b) => b,
            Err(e) => {
                eprintln!("failed fetching status from NEAR RPC node: {:?}", e);
                continue;
            }
        };
        let mut b = block_hash.write().unwrap();
        *b = current;
    }
}

#[actix_web::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let tera = Tera::new("templates/**/*").unwrap();

    let base_signer = InMemorySigner::from_file(&std::path::Path::new(&args.base_key_file))?;
    let rpc = JsonRpcClient::connect(&args.near_rpc_url);
    let nonce = match rpc
        .call(methods::query::RpcQueryRequest {
            block_reference: BlockReference::Finality(Finality::None),
            request: near_primitives::views::QueryRequest::ViewAccessKey {
                account_id: base_signer.account_id.clone(),
                public_key: base_signer.public_key.clone(),
            },
        })
        .await
    {
        Ok(r) => match r.kind {
            QueryResponseKind::AccessKey(a) => Arc::new(Mutex::new(a.nonce)),
            _ => anyhow::bail!(
                "received unexpected query response when getting access key info: {:?}",
                r.kind
            ),
        },
        Err(e) => {
            anyhow::bail!(
                "failed fetching access key info for {} {}: {:?}",
                &base_signer.account_id,
                &base_signer.public_key,
                e,
            );
        }
    };
    let block_hash = Arc::new(RwLock::new(current_block_hash(&rpc).await?));

    actix::spawn(update_block_hash(rpc.clone(), block_hash.clone()));

    let near_data = NearData {
        base_signer,
        nonce,
        block_hash,
        rpc,
    };
    println!("Starting server at: http://0.0.0.0:{}", args.port);
    // TODO: CORS to deny requests from other domains
    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(tera.clone()))
            .app_data(web::Data::new(near_data.clone()))
            .service(fs::Files::new("/assets", "assets").show_files_listing()) // for serving the static files
            .route("/", web::get().to(index))
            .route("/create_account", web::post().to(create_account))
    })
    .bind(("0.0.0.0", args.port))?
    .run()
    .await
    .map_err(Into::into)
}
