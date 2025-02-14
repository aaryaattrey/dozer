use dozer_types::{
    grpc_types::{
        app_ui::{
            code_service_server::{CodeService, CodeServiceServer},
            ConnectResponse, Label, Labels, RunRequest,
        },
        contract::{
            contract_service_server::{ContractService, ContractServiceServer},
            CommonRequest, DotResponse, SinkTablesRequest, SourcesRequest,
        },
        types::SchemasResponse,
    },
    log::info,
};
use futures::stream::BoxStream;
use metrics::IntoLabels;
use std::sync::Arc;
use tokio::sync::broadcast::Receiver;

use super::state::AppUIState;
use dozer_types::tracing::Level;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tower_http::trace::{self, TraceLayer};
pub const APP_UI_PORT: u16 = 4555;

struct ContractServer {
    state: Arc<AppUIState>,
}

#[tonic::async_trait]
impl ContractService for ContractServer {
    async fn sources(
        &self,
        request: Request<SourcesRequest>,
    ) -> Result<Response<SchemasResponse>, Status> {
        let req = request.into_inner();
        let res = self.state.get_source_schemas(req.connection_name).await;
        match res {
            Ok(res) => Ok(Response::new(res)),
            Err(e) => Err(Status::internal(e.to_string())),
        }
    }

    async fn sink_tables(
        &self,
        request: Request<SinkTablesRequest>,
    ) -> Result<Response<SchemasResponse>, Status> {
        let req = request.into_inner();
        let res = self.state.get_sink_table_schemas(req.sink_name).await;
        match res {
            Ok(res) => Ok(Response::new(res)),
            Err(e) => Err(Status::internal(e.to_string())),
        }
    }

    async fn generate_dot(
        &self,
        _request: Request<CommonRequest>,
    ) -> Result<Response<DotResponse>, Status> {
        let state = self.state.clone();
        let res = state.generate_dot().await;

        match res {
            Ok(res) => Ok(Response::new(res)),
            Err(e) => Err(Status::internal(e.to_string())),
        }
    }

    async fn get_graph_schemas(
        &self,
        _request: Request<CommonRequest>,
    ) -> Result<Response<SchemasResponse>, Status> {
        let state = self.state.clone();
        let res = state.get_graph_schemas().await;

        match res {
            Ok(res) => Ok(Response::new(res)),
            Err(e) => Err(Status::internal(e.to_string())),
        }
    }
}

struct AppUiServer {
    receiver: Receiver<ConnectResponse>,
    state: Arc<AppUIState>,
}

impl AppUiServer {
    pub fn new(receiver: Receiver<ConnectResponse>, state: Arc<AppUIState>) -> AppUiServer {
        Self { receiver, state }
    }
    async fn start(&self, req: RunRequest) -> Result<Response<Labels>, Status> {
        let state = self.state.clone();
        info!("Starting dozer");
        match state.run(req).await {
            Ok(labels) => {
                let labels = labels
                    .into_labels()
                    .into_iter()
                    .map(|label| Label {
                        key: label.key().to_string(),
                        value: label.value().to_string(),
                    })
                    .collect();
                Ok(Response::new(Labels { labels }))
            }
            Err(e) => Err(Status::internal(e.to_string())),
        }
    }
}

#[tonic::async_trait]
impl CodeService for AppUiServer {
    type AppUIConnectStream = BoxStream<'static, Result<ConnectResponse, Status>>;

    async fn app_ui_connect(
        &self,
        _request: Request<()>,
    ) -> Result<Response<Self::AppUIConnectStream>, Status> {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let mut receiver = self.receiver.resubscribe();

        let initial_state = self.state.clone();
        tokio::spawn(async move {
            let initial_state = initial_state.get_current().await;
            if let Err(e) = tx
                .send(Ok(ConnectResponse {
                    app_ui: Some(initial_state),
                    build: None,
                }))
                .await
            {
                info!("Error getting initial state");
                info!("{}", e.to_string());
                return {};
            }
            loop {
                let Ok(connect_response) = receiver.recv().await else {
                    break;
                };
                if tx.send(Ok(connect_response)).await.is_err() {
                    break;
                }
            }
        });
        let stream = ReceiverStream::new(rx);

        Ok(Response::new(Box::pin(stream) as Self::AppUIConnectStream))
    }

    async fn run(&self, request: Request<RunRequest>) -> Result<Response<Labels>, Status> {
        let req = request.into_inner();
        self.start(req).await
    }

    async fn stop(&self, _request: Request<()>) -> Result<Response<()>, Status> {
        let state = self.state.clone();
        info!("Stopping dozer");
        match state.stop().await {
            Ok(()) => Ok(Response::new(())),
            Err(e) => Err(Status::internal(e.to_string())),
        }
    }
}

pub async fn serve(
    receiver: Receiver<ConnectResponse>,
    state: Arc<AppUIState>,
) -> Result<(), tonic::transport::Error> {
    let addr = format!("0.0.0.0:{APP_UI_PORT}").parse().unwrap();
    let contract_server = ContractServer {
        state: state.clone(),
    };
    let app_ui_server = AppUiServer::new(receiver, state);
    let contract_service = ContractServiceServer::new(contract_server);
    let code_service = CodeServiceServer::new(app_ui_server);
    // Enable CORS for local development
    let contract_service = tonic_web::enable(contract_service);
    let code_service = tonic_web::enable(code_service);

    let reflection_service = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(
            dozer_types::grpc_types::contract::FILE_DESCRIPTOR_SET,
        )
        .register_encoded_file_descriptor_set(dozer_types::grpc_types::app_ui::FILE_DESCRIPTOR_SET)
        .build()
        .unwrap();

    tonic::transport::Server::builder()
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(trace::DefaultMakeSpan::new().level(Level::INFO))
                .on_response(trace::DefaultOnResponse::new().level(Level::INFO))
                .on_failure(trace::DefaultOnFailure::new().level(Level::ERROR)),
        )
        .accept_http1(true)
        .concurrency_limit_per_connection(32)
        .add_service(contract_service)
        .add_service(code_service)
        .add_service(reflection_service)
        .serve(addr)
        .await
}
