use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::BufReader;
use std::net::TcpStream;
use std::path::Path;
use std::str::FromStr;

use tera::Context as TeraContext;
use uuid::Uuid;

use crate::cloud_provider::environment::Environment;
use crate::cloud_provider::helm::{ChartInfo, ChartSetValue};
use crate::cloud_provider::kubernetes::Kubernetes;
use crate::cloud_provider::utilities::check_domain_for;
use crate::cloud_provider::DeploymentTarget;
use crate::cmd;
use crate::cmd::helm;
use crate::cmd::kubectl::ScalingKind::Statefulset;
use crate::cmd::kubectl::{
    kubectl_exec_delete_pod, kubectl_exec_delete_secret, kubectl_exec_get_pods,
    kubectl_exec_scale_replicas_by_selector, ScalingKind,
};
use crate::cmd::structs::{KubernetesPodStatusPhase, LabelsContent};
use crate::errors::{CommandError, EngineError};
use crate::events::{EngineEvent, EnvironmentStep, EventDetails, EventMessage, Stage, ToTransmitter};
use crate::io_models::ProgressLevel::Info;
use crate::io_models::{
    ApplicationAdvancedSettings, Context, DatabaseMode, Listen, Listeners, ListenersHelper, ProgressInfo,
    ProgressLevel, ProgressScope, QoveryIdentifier,
};
use crate::logger::Logger;

use crate::models::types::VersionsNumber;

// todo: delete this useless trait
pub trait Service: ToTransmitter {
    fn context(&self) -> &Context;
    fn service_type(&self) -> ServiceType;
    fn id(&self) -> &str;
    fn long_id(&self) -> &Uuid;
    fn name(&self) -> &str;
    fn sanitized_name(&self) -> String;
    fn name_with_id(&self) -> String {
        format!("{} ({})", self.name(), self.id())
    }
    fn name_with_id_and_version(&self) -> String {
        format!("{} ({}) version: {}", self.name(), self.id(), self.version())
    }
    fn workspace_directory(&self) -> String {
        let dir_root = match self.service_type() {
            ServiceType::Application => "applications",
            ServiceType::Database(_) => "databases",
            ServiceType::Router => "routers",
        };

        crate::fs::workspace_directory(
            self.context().workspace_root_dir(),
            self.context().execution_id(),
            format!("{}/{}", dir_root, self.name()),
        )
        .unwrap()
    }
    fn get_event_details(&self, stage: Stage) -> EventDetails {
        let context = self.context();
        EventDetails::new(
            None,
            QoveryIdentifier::from(context.organization_id().to_string()),
            QoveryIdentifier::from(context.cluster_id().to_string()),
            QoveryIdentifier::from(context.execution_id().to_string()),
            None,
            stage,
            self.to_transmitter(),
        )
    }
    fn application_advanced_settings(&self) -> Option<ApplicationAdvancedSettings>;
    fn version(&self) -> String;
    fn action(&self) -> &Action;
    fn private_port(&self) -> Option<u16>;
    fn total_cpus(&self) -> String;
    fn cpu_burst(&self) -> String;
    fn total_ram_in_mib(&self) -> u32;
    fn min_instances(&self) -> u32;
    fn max_instances(&self) -> u32;
    fn publicly_accessible(&self) -> bool;
    fn fqdn(&self, target: &DeploymentTarget, fqdn: &str, is_managed: bool) -> String {
        match &self.publicly_accessible() {
            true => fqdn.to_string(),
            false => match is_managed {
                true => format!("{}-dns.{}.svc.cluster.local", self.id(), target.environment.namespace()),
                false => format!("{}.{}.svc.cluster.local", self.sanitized_name(), target.environment.namespace()),
            },
        }
    }
    fn tera_context(&self, target: &DeploymentTarget) -> Result<TeraContext, EngineError>;
    // used to retrieve logs by using Kubernetes labels (selector)
    fn logger(&self) -> &dyn Logger;
    fn selector(&self) -> Option<String>;
    fn debug_logs(
        &self,
        deployment_target: &DeploymentTarget,
        event_details: EventDetails,
        logger: &dyn Logger,
    ) -> Vec<String> {
        debug_logs(self, deployment_target, event_details, logger)
    }
    fn is_listening(&self, ip: &str) -> bool {
        let private_port = match self.private_port() {
            Some(private_port) => private_port,
            _ => return false,
        };

        TcpStream::connect(format!("{}:{}", ip, private_port)).is_ok()
    }

    fn progress_scope(&self) -> ProgressScope {
        let id = self.id().to_string();

        match self.service_type() {
            ServiceType::Application => ProgressScope::Application { id },
            ServiceType::Database(_) => ProgressScope::Database { id },
            ServiceType::Router => ProgressScope::Router { id },
        }
    }
}

pub trait StatelessService: Service + Create + Pause + Delete {
    fn as_stateless_service(&self) -> &dyn StatelessService;
    fn exec_action(&self, deployment_target: &DeploymentTarget) -> Result<(), EngineError> {
        match self.action() {
            Action::Create => self.on_create(deployment_target),
            Action::Delete => self.on_delete(deployment_target),
            Action::Pause => self.on_pause(deployment_target),
            Action::Nothing => Ok(()),
        }
    }

    fn exec_check_action(&self) -> Result<(), EngineError> {
        match self.action() {
            Action::Create => self.on_create_check(),
            Action::Delete => self.on_delete_check(),
            Action::Pause => self.on_pause_check(),
            Action::Nothing => Ok(()),
        }
    }
}

pub trait RouterService: StatelessService + Listen + Helm {
    /// all domains (auto-generated by Qovery and user custom domains) associated to the router
    fn has_custom_domains(&self) -> bool;
    fn check_domains(
        &self,
        domains_to_check: Vec<&str>,
        event_details: EventDetails,
        logger: &dyn Logger,
    ) -> Result<(), EngineError> {
        check_domain_for(
            ListenersHelper::new(self.listeners()),
            domains_to_check,
            self.id(),
            self.context().execution_id(),
            event_details,
            logger,
        )?;
        Ok(())
    }
}

pub trait DatabaseService: Service + Create + Pause + Delete + Listen {
    fn check_domains(
        &self,
        listeners: Listeners,
        domains: Vec<&str>,
        event_details: EventDetails,
        logger: &dyn Logger,
    ) -> Result<(), EngineError> {
        if self.publicly_accessible() {
            check_domain_for(
                ListenersHelper::new(&listeners),
                domains,
                self.id(),
                self.context().execution_id(),
                event_details,
                logger,
            )?;
        }
        Ok(())
    }

    fn exec_action(&self, deployment_target: &DeploymentTarget) -> Result<(), EngineError> {
        match self.action() {
            Action::Create => self.on_create(deployment_target),
            Action::Delete => self.on_delete(deployment_target),
            Action::Pause => self.on_pause(deployment_target),
            Action::Nothing => Ok(()),
        }
    }

    fn exec_check_action(&self) -> Result<(), EngineError> {
        match self.action() {
            Action::Create => self.on_create_check(),
            Action::Delete => self.on_delete_check(),
            Action::Pause => self.on_pause_check(),
            Action::Nothing => Ok(()),
        }
    }

    fn is_managed_service(&self) -> bool;

    fn db_type(&self) -> DatabaseType;

    fn as_service(&self) -> &dyn Service;
}

pub trait Create {
    fn on_create(&self, target: &DeploymentTarget) -> Result<(), EngineError>;
    fn on_create_check(&self) -> Result<(), EngineError>;
    fn on_create_error(&self, target: &DeploymentTarget) -> Result<(), EngineError>;
}

pub trait Pause {
    fn on_pause(&self, target: &DeploymentTarget) -> Result<(), EngineError>;
    fn on_pause_check(&self) -> Result<(), EngineError>;
    fn on_pause_error(&self, target: &DeploymentTarget) -> Result<(), EngineError>;
}

pub trait Delete {
    fn on_delete(&self, target: &DeploymentTarget) -> Result<(), EngineError>;
    fn on_delete_check(&self) -> Result<(), EngineError>;
    fn on_delete_error(&self, target: &DeploymentTarget) -> Result<(), EngineError>;
}

pub trait Terraform {
    fn terraform_common_resource_dir_path(&self) -> String;
    fn terraform_resource_dir_path(&self) -> String;
}

pub trait Helm {
    fn helm_selector(&self) -> Option<String>;
    fn helm_release_name(&self) -> String;
    fn helm_chart_dir(&self) -> String;
    fn helm_chart_values_dir(&self) -> String;
    fn helm_chart_external_name_service_dir(&self) -> String;
}

#[derive(Clone, Eq, PartialEq, Hash)]
pub enum Action {
    Create,
    Pause,
    Delete,
    Nothing,
}

#[derive(Eq, PartialEq)]
pub struct DatabaseOptions {
    pub login: String,
    pub password: String,
    pub host: String,
    pub port: u16,
    pub mode: DatabaseMode,
    pub disk_size_in_gib: u32,
    pub database_disk_type: String,
    pub encrypt_disk: bool,
    pub activate_high_availability: bool,
    pub activate_backups: bool,
    pub publicly_accessible: bool,
}

#[derive(Debug, Eq, PartialEq, Copy, Clone, Serialize)]
pub enum DatabaseType {
    PostgreSQL,
    MongoDB,
    MySQL,
    Redis,
}

impl ToString for DatabaseType {
    fn to_string(&self) -> String {
        match self {
            DatabaseType::PostgreSQL => "PostgreSQL".to_string(),
            DatabaseType::MongoDB => "MongoDB".to_string(),
            DatabaseType::MySQL => "MySQL".to_string(),
            DatabaseType::Redis => "Redis".to_string(),
        }
    }
}

#[derive(Eq, PartialEq)]
pub enum ServiceType {
    Application,
    Database(DatabaseType),
    Router,
}

impl ServiceType {
    pub fn name(&self) -> String {
        match self {
            ServiceType::Application => "Application".to_string(),
            ServiceType::Database(db_type) => format!("{} database", db_type.to_string()),
            ServiceType::Router => "Router".to_string(),
        }
    }
}

impl ToString for ServiceType {
    fn to_string(&self) -> String {
        self.name()
    }
}

pub fn debug_logs<T>(
    service: &T,
    deployment_target: &DeploymentTarget,
    event_details: EventDetails,
    logger: &dyn Logger,
) -> Vec<String>
where
    T: Service + ?Sized,
{
    let kubernetes = deployment_target.kubernetes;
    let environment = deployment_target.environment;
    match get_stateless_resource_information_for_user(kubernetes, environment, service, event_details) {
        Ok(lines) => lines,
        Err(err) => {
            logger.log(EngineEvent::Error(
                err,
                Some(EventMessage::new_from_safe(format!(
                    "error while retrieving debug logs from {} {}",
                    service.service_type().name(),
                    service.name_with_id(),
                ))),
            ));

            Vec::new()
        }
    }
}

pub fn default_tera_context(
    service: &dyn Service,
    kubernetes: &dyn Kubernetes,
    environment: &Environment,
) -> TeraContext {
    let mut context = TeraContext::new();
    context.insert("id", service.id());
    context.insert("long_id", service.long_id());
    context.insert("owner_id", environment.owner_id.as_str());
    context.insert("project_id", environment.project_id.as_str());
    context.insert("project_long_id", &environment.project_long_id);
    context.insert("organization_id", environment.organization_id.as_str());
    context.insert("organization_long_id", &environment.organization_long_id);
    context.insert("environment_id", environment.id.as_str());
    context.insert("environment_long_id", &environment.long_id);
    context.insert("region", kubernetes.region().as_str());
    context.insert("zone", kubernetes.zone());
    context.insert("name", service.name());
    context.insert("sanitized_name", &service.sanitized_name());
    context.insert("namespace", environment.namespace());
    context.insert("cluster_name", kubernetes.name());
    context.insert("total_cpus", &service.total_cpus());
    context.insert("total_ram_in_mib", &service.total_ram_in_mib());
    context.insert("min_instances", &service.min_instances());
    context.insert("max_instances", &service.max_instances());

    context.insert("is_private_port", &service.private_port().is_some());
    if let Some(private_port) = service.private_port() {
        context.insert("private_port", &private_port);
    }

    context.insert("version", &service.version());

    context
}

/// deploy a stateless service created by the user (E.g: App or External Service)
/// the difference with `deploy_service(..)` is that this function provides the thrown error in case of failure
pub fn deploy_user_stateless_service<T>(target: &DeploymentTarget, service: &T) -> Result<(), EngineError>
where
    T: Service + Helm,
{
    deploy_stateless_service(target, service)
}

/// deploy a stateless service (app, router, database...) on Kubernetes
pub fn deploy_stateless_service<T>(target: &DeploymentTarget, service: &T) -> Result<(), EngineError>
where
    T: Service + Helm,
{
    let kubernetes = target.kubernetes;
    let environment = target.environment;
    let workspace_dir = service.workspace_directory();
    let tera_context = service.tera_context(target)?;
    let event_details = service.get_event_details(Stage::Environment(EnvironmentStep::Deploy));

    if let Err(e) = crate::template::generate_and_copy_all_files_into_dir(
        service.helm_chart_dir(),
        workspace_dir.as_str(),
        tera_context,
    ) {
        return Err(EngineError::new_cannot_copy_files_from_one_directory_to_another(
            event_details,
            service.helm_chart_dir(),
            workspace_dir,
            e,
        ));
    }

    let helm_release_name = service.helm_release_name();
    let kubernetes_config_file_path = kubernetes.get_kubeconfig_file_path()?;

    // define labels to add to namespace
    let namespace_labels = service.context().resource_expiration_in_seconds().map(|_| {
        vec![
            (LabelsContent {
                name: "ttl".to_string(),
                value: format! {"{}", service.context().resource_expiration_in_seconds().unwrap()},
            }),
        ]
    });

    // create a namespace with labels if do not exists
    cmd::kubectl::kubectl_exec_create_namespace(
        kubernetes_config_file_path.as_str(),
        environment.namespace(),
        namespace_labels,
        kubernetes.cloud_provider().credentials_environment_variables(),
    )
    .map_err(|e| {
        EngineError::new_k8s_create_namespace(event_details.clone(), environment.namespace().to_string(), e)
    })?;

    // do exec helm upgrade and return the last deployment status
    let helm = helm::Helm::new(
        &kubernetes_config_file_path,
        &kubernetes.cloud_provider().credentials_environment_variables(),
    )
    .map_err(|e| helm::to_engine_error(&event_details, e))?;
    let chart = ChartInfo::new_from_custom_namespace(
        helm_release_name,
        workspace_dir.clone(),
        environment.namespace().to_string(),
        600_i64,
        match service.service_type() {
            ServiceType::Database(_) => vec![format!("{}/q-values.yaml", &workspace_dir)],
            _ => vec![],
        },
        vec![],
        vec![],
        false,
        service.selector(),
    );

    helm.upgrade(&chart, &[])
        .map_err(|e| helm::to_engine_error(&event_details, e))?;

    delete_pending_service(
        kubernetes_config_file_path.as_str(),
        environment.namespace(),
        service.selector().unwrap_or_default().as_str(),
        kubernetes.cloud_provider().credentials_environment_variables(),
        event_details.clone(),
    )?;

    cmd::kubectl::kubectl_exec_is_pod_ready_with_retry(
        kubernetes_config_file_path.as_str(),
        environment.namespace(),
        service.selector().unwrap_or_default().as_str(),
        kubernetes.cloud_provider().credentials_environment_variables(),
    )
    .map_err(|e| {
        EngineError::new_k8s_pod_not_ready(
            event_details.clone(),
            service.selector().unwrap_or_default(),
            environment.namespace().to_string(),
            e,
        )
    })?;

    Ok(())
}

/// do specific operations on a stateless service deployment error
pub fn deploy_stateless_service_error<T>(_target: &DeploymentTarget, _service: &T) -> Result<(), EngineError>
where
    T: Service + Helm,
{
    // Nothing to do as we sait --atomic on chart release that we do
    // So helm rollback for us if a deployment fails
    Ok(())
}

pub fn scale_down_database(
    target: &DeploymentTarget,
    service: &impl DatabaseService,
    replicas_count: usize,
) -> Result<(), EngineError> {
    if service.is_managed_service() {
        // Doing nothing for pause database as it is a managed service
        return Ok(());
    }

    let event_details = service.get_event_details(Stage::Environment(EnvironmentStep::ScaleDown));
    let kubernetes = target.kubernetes;
    let environment = target.environment;
    let kubernetes_config_file_path = kubernetes.get_kubeconfig_file_path()?;

    let selector = format!("databaseId={}", service.id());
    kubectl_exec_scale_replicas_by_selector(
        kubernetes_config_file_path,
        kubernetes.cloud_provider().credentials_environment_variables(),
        environment.namespace(),
        Statefulset,
        selector.as_str(),
        replicas_count as u32,
    )
    .map_err(|e| {
        EngineError::new_k8s_scale_replicas(
            event_details.clone(),
            selector.to_string(),
            environment.namespace().to_string(),
            replicas_count as u32,
            e,
        )
    })
}

pub fn scale_down_application(
    target: &DeploymentTarget,
    service: &impl StatelessService,
    replicas_count: usize,
    scaling_kind: ScalingKind,
) -> Result<(), EngineError> {
    let event_details = service.get_event_details(Stage::Environment(EnvironmentStep::ScaleDown));
    let kubernetes = target.kubernetes;
    let environment = target.environment;
    let kubernetes_config_file_path = kubernetes.get_kubeconfig_file_path()?;

    kubectl_exec_scale_replicas_by_selector(
        kubernetes_config_file_path,
        kubernetes.cloud_provider().credentials_environment_variables(),
        environment.namespace(),
        scaling_kind,
        service.selector().unwrap_or_default().as_str(),
        replicas_count as u32,
    )
    .map_err(|e| {
        EngineError::new_k8s_scale_replicas(
            event_details.clone(),
            service.selector().unwrap_or_default(),
            environment.namespace().to_string(),
            replicas_count as u32,
            e,
        )
    })
}

pub fn delete_stateless_service<T>(
    target: &DeploymentTarget,
    service: &T,
    event_details: EventDetails,
) -> Result<(), EngineError>
where
    T: Service + Helm,
{
    let kubernetes = target.kubernetes;
    let environment = target.environment;
    let helm_release_name = service.helm_release_name();

    // clean the resource
    helm_uninstall_release(kubernetes, environment, helm_release_name.as_str(), event_details)?;

    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseTerraformConfig {
    #[serde(rename = "database_target_id")]
    pub target_id: String,
    #[serde(rename = "database_target_hostname")]
    pub target_hostname: String,
    #[serde(rename = "database_target_fqdn_id")]
    pub target_fqdn_id: String,
    #[serde(rename = "database_target_fqdn")]
    pub target_fqdn: String,
}

pub enum DatabaseTerraformConfigError {
    FileDoesntExist(CommandError),
    FileCannotBeParsed(CommandError),
}

pub fn get_database_terraform_config(
    database_terraform_config_file: &str,
) -> Result<DatabaseTerraformConfig, DatabaseTerraformConfigError> {
    let file_content = match File::open(&database_terraform_config_file) {
        Ok(f) => f,
        Err(e) => {
            return Err(DatabaseTerraformConfigError::FileDoesntExist(CommandError::new(
                format!("Cannot open database config file: {}", database_terraform_config_file),
                Some(e.to_string()),
                None,
            )));
        }
    };

    let reader = BufReader::new(file_content);
    match serde_json::from_reader(reader) {
        Ok(config) => Ok(config),
        Err(e) => Err(DatabaseTerraformConfigError::FileCannotBeParsed(CommandError::new(
            format!(
                "Error while parsing database terraform config file {}",
                database_terraform_config_file
            ),
            Some(e.to_string()),
            None,
        ))),
    }
}

pub fn deploy_stateful_service<T>(
    target: &DeploymentTarget,
    service: &T,
    event_details: EventDetails,
    logger: &dyn Logger,
) -> Result<(), EngineError>
where
    T: DatabaseService + Helm + Terraform,
{
    let workspace_dir = service.workspace_directory();
    let kubernetes = target.kubernetes;
    let environment = target.environment;

    let _context = service.tera_context(target)?;
    let kubernetes_config_file_path = kubernetes.get_kubeconfig_file_path()?;

    // define labels to add to namespace
    let namespace_labels = service.context().resource_expiration_in_seconds().map(|_| {
        vec![
            (LabelsContent {
                name: "ttl".into(),
                value: format!("{}", service.context().resource_expiration_in_seconds().unwrap()),
            }),
        ]
    });

    // create a namespace with labels if it does not exist
    cmd::kubectl::kubectl_exec_create_namespace(
        &kubernetes_config_file_path,
        environment.namespace(),
        namespace_labels,
        kubernetes.cloud_provider().credentials_environment_variables(),
    )
    .map_err(|e| {
        EngineError::new_k8s_create_namespace(event_details.clone(), environment.namespace().to_string(), e)
    })?;

    // do exec helm upgrade and return the last deployment status
    let helm = helm::Helm::new(
        &kubernetes_config_file_path,
        &kubernetes.cloud_provider().credentials_environment_variables(),
    )
    .map_err(|e| helm::to_engine_error(&event_details, e))?;

    if service.is_managed_service() {
        logger.log(EngineEvent::Info(
            event_details.clone(),
            EventMessage::new_from_safe(format!(
                "Deploying managed {} `{}`",
                service.service_type().name(),
                service.name_with_id()
            )),
        ));

        let context = service.tera_context(target)?;

        if let Err(e) = crate::template::generate_and_copy_all_files_into_dir(
            service.terraform_common_resource_dir_path(),
            &workspace_dir,
            context.clone(),
        ) {
            return Err(EngineError::new_cannot_copy_files_from_one_directory_to_another(
                event_details,
                service.terraform_common_resource_dir_path(),
                workspace_dir,
                e,
            ));
        }

        if let Err(e) = crate::template::generate_and_copy_all_files_into_dir(
            service.terraform_resource_dir_path(),
            &workspace_dir,
            context.clone(),
        ) {
            return Err(EngineError::new_cannot_copy_files_from_one_directory_to_another(
                event_details,
                service.terraform_resource_dir_path(),
                workspace_dir,
                e,
            ));
        }

        let external_svc_dir = format!("{}/{}", workspace_dir, "external-name-svc");
        if let Err(e) = crate::template::generate_and_copy_all_files_into_dir(
            service.helm_chart_external_name_service_dir(),
            external_svc_dir.as_str(),
            context,
        ) {
            return Err(EngineError::new_cannot_copy_files_from_one_directory_to_another(
                event_details,
                service.helm_chart_external_name_service_dir(),
                external_svc_dir,
                e,
            ));
        }

        cmd::terraform::terraform_init_validate_plan_apply(
            workspace_dir.as_str(),
            service.context().is_dry_run_deploy(),
        )
        .map_err(|e| EngineError::new_terraform_error_while_executing_pipeline(event_details.clone(), e))?;

        // Gather database TF generated JSON config if any
        // if configuration exists, it means that HELM will be used to deploy managed DB service external name chart instead on Terraform
        match get_database_terraform_config(format!("{}/database-tf-config.json", workspace_dir).as_str()) {
            Ok(database_config) => {
                // Deploying helm chart
                let chart = ChartInfo::new_from_custom_namespace(
                    format!("{}-externalname", database_config.target_id),
                    external_svc_dir,
                    environment.namespace().to_string(),
                    600_i64,
                    vec![],
                    vec![
                        ChartSetValue {
                            key: "target_hostname".to_string(),
                            value: database_config.target_hostname,
                        },
                        ChartSetValue {
                            key: "source_fqdn".to_string(),
                            value: database_config.target_fqdn,
                        },
                        ChartSetValue {
                            key: "app_id".to_string(),
                            value: service.id().to_string(),
                        },
                        ChartSetValue {
                            key: "service_name".to_string(),
                            value: database_config.target_fqdn_id,
                        },
                        ChartSetValue {
                            key: "publicly_accessible".to_string(),
                            value: service.publicly_accessible().to_string(),
                        },
                    ],
                    vec![],
                    false,
                    service.selector(),
                );

                helm.upgrade(&chart, &[])
                    .map_err(|e| helm::to_engine_error(&event_details, e))?;
            }
            Err(e) => {
                match e {
                    DatabaseTerraformConfigError::FileDoesntExist(_) => {
                        // Service External name for DB creation is supposed to be handled by Terraform
                        // do nothing
                    }
                    DatabaseTerraformConfigError::FileCannotBeParsed(e) => {
                        // Database config file is here but cannot be parsed, it's an issue
                        return Err(EngineError::new_terraform_database_config_mismatch(event_details, e));
                    }
                }
            }
        }
    } else {
        // use helm
        logger.log(EngineEvent::Info(
            event_details.clone(),
            EventMessage::new_from_safe(format!(
                "Deploying containerized {} `{}` on Kubernetes cluster",
                service.service_type().name(),
                service.name_with_id()
            )),
        ));

        let context = service.tera_context(target)?;
        let kubernetes_config_file_path = kubernetes.get_kubeconfig_file_path()?;

        // default chart
        if let Err(e) = crate::template::generate_and_copy_all_files_into_dir(
            service.helm_chart_dir(),
            workspace_dir.as_str(),
            context.clone(),
        ) {
            return Err(EngineError::new_cannot_copy_files_from_one_directory_to_another(
                event_details,
                service.helm_chart_dir(),
                workspace_dir,
                e,
            ));
        };

        // overwrite with our chart values
        if let Err(e) = crate::template::generate_and_copy_all_files_into_dir(
            service.helm_chart_values_dir(),
            workspace_dir.as_str(),
            context,
        ) {
            return Err(EngineError::new_cannot_copy_files_from_one_directory_to_another(
                event_details,
                service.helm_chart_values_dir(),
                workspace_dir,
                e,
            ));
        };

        // define labels to add to namespace
        let namespace_labels = service.context().resource_expiration_in_seconds().map(|_| {
            vec![
                (LabelsContent {
                    name: "ttl".into(),
                    value: format!("{}", service.context().resource_expiration_in_seconds().unwrap()),
                }),
            ]
        });

        // create a namespace with labels if it does not exist
        cmd::kubectl::kubectl_exec_create_namespace(
            &kubernetes_config_file_path,
            environment.namespace(),
            namespace_labels,
            kubernetes.cloud_provider().credentials_environment_variables(),
        )
        .map_err(|e| {
            EngineError::new_k8s_create_namespace(event_details.clone(), environment.namespace().to_string(), e)
        })?;

        // do exec helm upgrade and return the last deployment status
        let helm = helm::Helm::new(
            &kubernetes_config_file_path,
            &kubernetes.cloud_provider().credentials_environment_variables(),
        )
        .map_err(|e| helm::to_engine_error(&event_details, e))?;
        let chart = ChartInfo::new_from_custom_namespace(
            service.helm_release_name(),
            workspace_dir.clone(),
            environment.namespace().to_string(),
            600_i64,
            match service.service_type() {
                ServiceType::Database(_) => vec![format!("{}/q-values.yaml", &workspace_dir)],
                _ => vec![],
            },
            vec![],
            vec![],
            false,
            service.selector(),
        );

        helm.upgrade(&chart, &[])
            .map_err(|e| helm::to_engine_error(&event_details, e))?;

        delete_pending_service(
            kubernetes_config_file_path.as_str(),
            environment.namespace(),
            service.selector().unwrap_or_default().as_str(),
            kubernetes.cloud_provider().credentials_environment_variables(),
            event_details.clone(),
        )?;

        // check app status
        let is_pod_ready = cmd::kubectl::kubectl_exec_is_pod_ready_with_retry(
            &kubernetes_config_file_path,
            environment.namespace(),
            service.selector().unwrap_or_default().as_str(),
            kubernetes.cloud_provider().credentials_environment_variables(),
        );
        if let Ok(Some(true)) = is_pod_ready {
            return Ok(());
        }

        return Err(EngineError::new_database_failed_to_start_after_several_retries(
            event_details,
            service.name_with_id(),
            service.service_type().name(),
            match is_pod_ready {
                Err(e) => Some(e),
                _ => None,
            },
        ));
    }

    Ok(())
}

pub fn delete_stateful_service<T>(
    target: &DeploymentTarget,
    service: &T,
    event_details: EventDetails,
    logger: &dyn Logger,
) -> Result<(), EngineError>
where
    T: DatabaseService + Helm + Terraform,
{
    let kubernetes = target.kubernetes;
    let environment = target.environment;
    if service.is_managed_service() {
        let workspace_dir = service.workspace_directory();
        let tera_context = service.tera_context(target)?;

        if let Err(e) = crate::template::generate_and_copy_all_files_into_dir(
            service.terraform_common_resource_dir_path(),
            workspace_dir.as_str(),
            tera_context.clone(),
        ) {
            return Err(EngineError::new_cannot_copy_files_from_one_directory_to_another(
                event_details,
                service.terraform_common_resource_dir_path(),
                workspace_dir,
                e,
            ));
        }

        if let Err(e) = crate::template::generate_and_copy_all_files_into_dir(
            service.terraform_resource_dir_path(),
            workspace_dir.as_str(),
            tera_context.clone(),
        ) {
            return Err(EngineError::new_cannot_copy_files_from_one_directory_to_another(
                event_details,
                service.terraform_resource_dir_path(),
                workspace_dir,
                e,
            ));
        }

        let external_svc_dir = format!("{}/{}", workspace_dir, "external-name-svc");
        if let Err(e) = crate::template::generate_and_copy_all_files_into_dir(
            service.helm_chart_external_name_service_dir(),
            &external_svc_dir,
            tera_context.clone(),
        ) {
            return Err(EngineError::new_cannot_copy_files_from_one_directory_to_another(
                event_details,
                service.helm_chart_external_name_service_dir(),
                external_svc_dir,
                e,
            ));
        }

        if let Err(e) = crate::template::generate_and_copy_all_files_into_dir(
            service.helm_chart_external_name_service_dir(),
            workspace_dir.as_str(),
            tera_context,
        ) {
            return Err(EngineError::new_cannot_copy_files_from_one_directory_to_another(
                event_details,
                service.helm_chart_external_name_service_dir(),
                workspace_dir,
                e,
            ));
        }

        match cmd::terraform::terraform_init_validate_destroy(workspace_dir.as_str(), true) {
            Ok(_) => {
                logger.log(EngineEvent::Info(
                    event_details,
                    EventMessage::new_from_safe("Deleting secret containing tfstates".to_string()),
                ));
                let _ =
                    delete_terraform_tfstate_secret(kubernetes, environment.namespace(), &get_tfstate_name(service));
            }
            Err(e) => {
                let engine_err = EngineError::new_terraform_error_while_executing_destroy_pipeline(event_details, e);

                logger.log(EngineEvent::Error(engine_err.clone(), None));

                return Err(engine_err);
            }
        }
    } else {
        // If not managed, we use helm to deploy
        let helm_release_name = service.helm_release_name();
        // clean the resource
        helm_uninstall_release(kubernetes, environment, helm_release_name.as_str(), event_details)?;
    }

    Ok(())
}

pub struct ServiceVersionCheckResult {
    requested_version: VersionsNumber,
    matched_version: VersionsNumber,
    message: Option<String>,
}

impl ServiceVersionCheckResult {
    pub fn new(requested_version: VersionsNumber, matched_version: VersionsNumber, message: Option<String>) -> Self {
        ServiceVersionCheckResult {
            requested_version,
            matched_version,
            message,
        }
    }

    pub fn matched_version(&self) -> VersionsNumber {
        self.matched_version.clone()
    }

    pub fn requested_version(&self) -> &VersionsNumber {
        &self.requested_version
    }

    pub fn message(&self) -> Option<String> {
        self.message.clone()
    }
}

pub fn check_service_version<T>(
    result: Result<String, CommandError>,
    service: &T,
    event_details: EventDetails,
    logger: &dyn Logger,
) -> Result<ServiceVersionCheckResult, EngineError>
where
    T: Service + Listen,
{
    let listeners_helper = ListenersHelper::new(service.listeners());

    match result {
        Ok(version) => {
            if service.version() != version.as_str() {
                let message = format!(
                    "{} version `{}` has been requested by the user; but matching version is `{}`",
                    service.service_type().name(),
                    service.version(),
                    version.as_str()
                );

                logger.log(EngineEvent::Info(
                    event_details.clone(),
                    EventMessage::new_from_safe(message.to_string()),
                ));

                let progress_info = ProgressInfo::new(
                    service.progress_scope(),
                    Info,
                    Some(message.to_string()),
                    service.context().execution_id(),
                );

                listeners_helper.deployment_in_progress(progress_info);

                return Ok(ServiceVersionCheckResult::new(
                    VersionsNumber::from_str(&service.version()).map_err(|e| {
                        EngineError::new_version_number_parsing_error(event_details.clone(), service.version(), e)
                    })?,
                    VersionsNumber::from_str(&version).map_err(|e| {
                        EngineError::new_version_number_parsing_error(event_details.clone(), version.to_string(), e)
                    })?,
                    Some(message),
                ));
            }

            Ok(ServiceVersionCheckResult::new(
                VersionsNumber::from_str(&service.version()).map_err(|e| {
                    EngineError::new_version_number_parsing_error(event_details.clone(), service.version(), e)
                })?,
                VersionsNumber::from_str(&version).map_err(|e| {
                    EngineError::new_version_number_parsing_error(event_details.clone(), version.to_string(), e)
                })?,
                None,
            ))
        }
        Err(_err) => {
            let message = format!(
                "{} version {} is not supported!",
                service.service_type().name(),
                service.version(),
            );

            let progress_info = ProgressInfo::new(
                service.progress_scope(),
                ProgressLevel::Error,
                Some(message),
                service.context().execution_id(),
            );

            listeners_helper.deployment_error(progress_info);

            let error = EngineError::new_unsupported_version_error(
                event_details,
                service.service_type().name(),
                service.version(),
            );

            logger.log(EngineEvent::Error(error.clone(), None));

            Err(error)
        }
    }
}

fn delete_terraform_tfstate_secret(
    kubernetes: &dyn Kubernetes,
    namespace: &str,
    secret_name: &str,
) -> Result<(), EngineError> {
    let config_file_path = kubernetes.get_kubeconfig_file_path()?;

    // create the namespace to insert the tfstate in secrets
    let _ = kubectl_exec_delete_secret(
        config_file_path,
        namespace,
        secret_name,
        kubernetes.cloud_provider().credentials_environment_variables(),
    );

    Ok(())
}

pub enum CheckAction {
    Deploy,
    Pause,
    Delete,
}

pub fn check_kubernetes_service_error<T>(
    result: Result<(), EngineError>,
    kubernetes: &dyn Kubernetes,
    service: &T,
    event_details: EventDetails,
    logger: &dyn Logger,
    deployment_target: &DeploymentTarget,
    listeners_helper: &ListenersHelper,
    action_verb: &str,
    action: CheckAction,
) -> Result<(), EngineError>
where
    T: Service + ?Sized,
{
    let message = format!(
        "{} {} {}",
        action_verb,
        service.service_type().name().to_lowercase(),
        service.name()
    );

    let progress_info = ProgressInfo::new(
        service.progress_scope(),
        Info,
        Some(message.to_string()),
        kubernetes.context().execution_id(),
    );

    match action {
        CheckAction::Deploy => {
            listeners_helper.deployment_in_progress(progress_info);
            logger.log(EngineEvent::Info(event_details.clone(), EventMessage::new_from_safe(message)));
        }
        CheckAction::Pause => {
            listeners_helper.pause_in_progress(progress_info);
            logger.log(EngineEvent::Info(event_details.clone(), EventMessage::new_from_safe(message)));
        }
        CheckAction::Delete => {
            listeners_helper.delete_in_progress(progress_info);
            logger.log(EngineEvent::Info(event_details.clone(), EventMessage::new_from_safe(message)));
        }
    }

    match result {
        Err(err) => {
            let progress_info = ProgressInfo::new(
                service.progress_scope(),
                ProgressLevel::Error,
                Some(format!(
                    "{} error {} {} : error => {:?}",
                    action_verb,
                    service.service_type().name().to_lowercase(),
                    service.name(),
                    // Note: env vars are not leaked to legacy listeners since it can holds sensitive data
                    // such as secrets and such.
                    err
                )),
                kubernetes.context().execution_id(),
            );

            logger.log(EngineEvent::Error(
                err.clone(),
                Some(EventMessage::new_from_safe(format!(
                    "{} error with {} {} , id: {}",
                    action_verb,
                    service.service_type().name(),
                    service.name(),
                    service.id(),
                ))),
            ));

            match action {
                CheckAction::Deploy => listeners_helper.deployment_error(progress_info),
                CheckAction::Pause => listeners_helper.pause_error(progress_info),
                CheckAction::Delete => listeners_helper.delete_error(progress_info),
            }

            let debug_logs = service.debug_logs(deployment_target, event_details.clone(), logger);
            let debug_logs_string = if !debug_logs.is_empty() {
                debug_logs.join("\n")
            } else {
                String::from("<no debug logs>")
            };

            let progress_info = ProgressInfo::new(
                service.progress_scope(),
                ProgressLevel::Debug,
                Some(debug_logs_string.to_string()),
                kubernetes.context().execution_id(),
            );

            logger.log(EngineEvent::Debug(
                event_details.clone(),
                EventMessage::new_from_safe(debug_logs_string),
            ));

            match action {
                CheckAction::Deploy => listeners_helper.deployment_error(progress_info),
                CheckAction::Pause => listeners_helper.pause_error(progress_info),
                CheckAction::Delete => listeners_helper.delete_error(progress_info),
            }

            Err(EngineError::new_k8s_service_issue(
                event_details,
                err.underlying_error().unwrap_or_default(),
            ))
        }
        _ => {
            let progress_info = ProgressInfo::new(
                service.progress_scope(),
                Info,
                Some(format!(
                    "{} succeeded for {} {}",
                    action_verb,
                    service.service_type().name().to_lowercase(),
                    service.name()
                )),
                kubernetes.context().execution_id(),
            );

            match action {
                CheckAction::Deploy => listeners_helper.deployment_in_progress(progress_info),
                CheckAction::Pause => listeners_helper.pause_in_progress(progress_info),
                CheckAction::Delete => listeners_helper.delete_in_progress(progress_info),
            }

            Ok(())
        }
    }
}

pub type Logs = String;
pub type Describe = String;

/// return debug information line by line to help the user to understand what's going on,
/// and why its app does not start
pub fn get_stateless_resource_information_for_user<T>(
    kubernetes: &dyn Kubernetes,
    environment: &Environment,
    service: &T,
    event_details: EventDetails,
) -> Result<Vec<String>, EngineError>
where
    T: Service + ?Sized,
{
    let selector = service.selector().unwrap_or_default();
    let mut result = Vec::with_capacity(50);
    let kubernetes_config_file_path = kubernetes.get_kubeconfig_file_path()?;

    // get logs
    let logs = cmd::kubectl::kubectl_exec_logs(
        &kubernetes_config_file_path,
        environment.namespace(),
        selector.as_str(),
        kubernetes.cloud_provider().credentials_environment_variables(),
    )
    .map_err(|e| {
        EngineError::new_k8s_get_logs_error(
            event_details.clone(),
            selector.to_string(),
            environment.namespace().to_string(),
            e,
        )
    })?;

    result.extend(logs);

    // get pod state
    let pods = kubectl_exec_get_pods(
        &kubernetes_config_file_path,
        Some(environment.namespace()),
        Some(selector.as_str()),
        kubernetes.cloud_provider().credentials_environment_variables(),
    )
    .map_err(|e| EngineError::new_k8s_cannot_get_pods(event_details.clone(), e))?
    .items;

    for pod in pods {
        for container_condition in pod.status.conditions {
            if container_condition.status.to_ascii_lowercase() == "false" {
                result.push(format!(
                    "Condition not met to start the container: {} -> {:?}: {}",
                    container_condition.typee,
                    container_condition.reason,
                    container_condition.message.unwrap_or_default()
                ))
            }
        }
        for container_status in pod.status.container_statuses.unwrap_or_default() {
            if let Some(last_state) = container_status.last_state {
                if let Some(terminated) = last_state.terminated {
                    if let Some(message) = terminated.message {
                        result.push(format!("terminated state message: {}", message));
                    }

                    result.push(format!("terminated state exit code: {}", terminated.exit_code));
                }

                if let Some(waiting) = last_state.waiting {
                    if let Some(message) = waiting.message {
                        result.push(format!("waiting state message: {}", message));
                    }
                }
            }
        }
    }

    // get pod events
    let events = cmd::kubectl::kubectl_exec_get_json_events(
        &kubernetes_config_file_path,
        environment.namespace(),
        kubernetes.cloud_provider().credentials_environment_variables(),
    )
    .map_err(|e| EngineError::new_k8s_get_json_events(event_details.clone(), environment.namespace().to_string(), e))?
    .items;

    for event in events {
        if event.type_.to_lowercase() != "normal" {
            if let Some(message) = event.message {
                result.push(format!(
                    "{} {} {}: {}",
                    event.last_timestamp.unwrap_or_default(),
                    event.type_,
                    event.reason,
                    message
                ));
            }
        }
    }

    Ok(result)
}

pub fn helm_uninstall_release(
    kubernetes: &dyn Kubernetes,
    environment: &Environment,
    helm_release_name: &str,
    event_details: EventDetails,
) -> Result<(), EngineError> {
    let kubernetes_config_file_path = kubernetes.get_kubeconfig_file_path()?;

    let helm = helm::Helm::new(
        &kubernetes_config_file_path,
        &kubernetes.cloud_provider().credentials_environment_variables(),
    )
    .map_err(|e| EngineError::new_helm_error(event_details.clone(), e))?;

    let chart = ChartInfo::new_from_release_name(helm_release_name, environment.namespace());
    helm.uninstall(&chart, &[])
        .map_err(|e| EngineError::new_helm_error(event_details.clone(), e))
}

pub fn get_tfstate_suffix(service: &dyn Service) -> String {
    service.id().to_string()
}

// Name generated from TF secret suffix
// https://www.terraform.io/docs/backends/types/kubernetes.html#secret_suffix
// As mention the doc: Secrets will be named in the format: tfstate-{workspace}-{secret_suffix}.
pub fn get_tfstate_name(service: &dyn Service) -> String {
    format!("tfstate-default-{}", service.id())
}

fn delete_pending_service<P>(
    kubernetes_config: P,
    namespace: &str,
    selector: &str,
    envs: Vec<(&str, &str)>,
    event_details: EventDetails,
) -> Result<(), EngineError>
where
    P: AsRef<Path>,
{
    match kubectl_exec_get_pods(&kubernetes_config, Some(namespace), Some(selector), envs.clone()) {
        Ok(pods) => {
            for pod in pods.items {
                if pod.status.phase == KubernetesPodStatusPhase::Pending {
                    if let Err(e) = kubectl_exec_delete_pod(
                        &kubernetes_config,
                        pod.metadata.namespace.as_str(),
                        pod.metadata.name.as_str(),
                        envs.clone(),
                    ) {
                        return Err(EngineError::new_k8s_service_issue(event_details, e));
                    }
                }
            }

            Ok(())
        }
        Err(e) => Err(EngineError::new_k8s_service_issue(event_details, e)),
    }
}
