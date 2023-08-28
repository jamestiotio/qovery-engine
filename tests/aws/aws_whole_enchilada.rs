use crate::helpers;
use crate::helpers::common::ClusterDomain;
use crate::helpers::kubernetes::{cluster_test, ClusterTestType};
use crate::helpers::utilities::{
    context_for_cluster, engine_run_test, generate_cluster_id, generate_id, logger, metrics_registry, FuncTestsSecrets,
};
use ::function_name::named;
use qovery_engine::cloud_provider::aws::kubernetes::VpcQoveryNetworkMode::WithNatGateways;
use qovery_engine::cloud_provider::aws::regions::AwsRegion;
use qovery_engine::cloud_provider::kubernetes::Kind as KKind;
use qovery_engine::cloud_provider::models::CpuArchitecture;
use qovery_engine::cloud_provider::Kind;
use qovery_engine::utilities::to_short_id;
use std::str::FromStr;

#[cfg(feature = "test-aws-whole-enchilada")]
#[named]
#[test]
fn create_and_destroy_eks_cluster_with_env_in_eu_west_3() {
    let secrets = FuncTestsSecrets::new();

    let region = secrets
        .AWS_DEFAULT_REGION
        .as_ref()
        .expect("AWS region was not found in secrets");
    let aws_region = AwsRegion::from_str(region).expect("Wasn't able to parse the desired region");
    let aws_zones = aws_region.get_zones();

    let organization_id = generate_id();
    let cluster_id = generate_cluster_id(aws_region.to_string().as_str());
    let context = context_for_cluster(organization_id, cluster_id, Some(KKind::Eks));

    let cluster_domain = format!(
        "{}.{}",
        to_short_id(&cluster_id),
        secrets
            .DEFAULT_TEST_DOMAIN
            .as_ref()
            .expect("DEFAULT_TEST_DOMAIN is not set in secrets")
            .as_str()
    );

    let environment = helpers::environment::working_minimal_environment(&context);
    let env_action = environment;

    engine_run_test(|| {
        cluster_test(
            function_name!(),
            Kind::Aws,
            KKind::Eks,
            context.clone(),
            logger(),
            metrics_registry(),
            region,
            Some(aws_zones),
            ClusterTestType::Classic,
            &ClusterDomain::Custom { domain: cluster_domain },
            Some(WithNatGateways),
            CpuArchitecture::AMD64,
            Some(&env_action),
        )
    })
}

#[cfg(feature = "test-aws-whole-enchilada")]
#[named]
#[test]
fn create_resize_and_destroy_eks_cluster_with_env_in_eu_west_3() {
    let secrets = FuncTestsSecrets::new();

    let region = secrets
        .AWS_DEFAULT_REGION
        .as_ref()
        .expect("AWS region was not found in secrets");
    let aws_region = AwsRegion::from_str(region).expect("Wasn't able to convert the desired region");
    let aws_zones = aws_region.get_zones();

    let organization_id = generate_id();
    let cluster_id = generate_cluster_id(aws_region.to_string().as_str());
    let context = context_for_cluster(organization_id, cluster_id, Some(KKind::Eks));

    let cluster_domain = format!(
        "{}.{}",
        to_short_id(&cluster_id),
        secrets
            .DEFAULT_TEST_DOMAIN
            .as_ref()
            .expect("DEFAULT_TEST_DOMAIN is not set in secrets")
            .as_str()
    );

    engine_run_test(|| {
        cluster_test(
            function_name!(),
            Kind::Aws,
            KKind::Eks,
            context.clone(),
            logger(),
            metrics_registry(),
            region,
            Some(aws_zones),
            ClusterTestType::WithNodesResize,
            &ClusterDomain::Custom { domain: cluster_domain },
            None,
            CpuArchitecture::AMD64,
            None,
        )
    })
}

#[cfg(feature = "test-aws-whole-enchilada")]
#[ignore]
#[named]
#[test]
fn create_pause_and_destroy_eks_cluster_with_env_in_eu_west_3() {
    let secrets = FuncTestsSecrets::new();

    let region = secrets.AWS_DEFAULT_REGION.as_ref().expect("AWS region was not found");
    let aws_region = AwsRegion::from_str(region).expect("Wasn't able to parse the desired region");
    let aws_zones = aws_region.get_zones();

    let organization_id = generate_id();
    let cluster_id = generate_cluster_id(aws_region.to_string().as_str());
    let context = context_for_cluster(organization_id, cluster_id, Some(KKind::Eks));

    let cluster_domain = format!(
        "{}.{}",
        to_short_id(&cluster_id),
        secrets
            .DEFAULT_TEST_DOMAIN
            .as_ref()
            .expect("DEFAULT_TEST_DOMAIN is not set in secrets")
            .as_str()
    );

    let environment = helpers::environment::working_minimal_environment(&context);
    let env_action = environment;

    engine_run_test(|| {
        cluster_test(
            function_name!(),
            Kind::Aws,
            KKind::Eks,
            context.clone(),
            logger(),
            metrics_registry(),
            region,
            Some(aws_zones),
            ClusterTestType::WithPause,
            &ClusterDomain::Custom { domain: cluster_domain },
            Some(WithNatGateways),
            CpuArchitecture::AMD64,
            Some(&env_action),
        )
    })
}

#[cfg(feature = "test-aws-whole-enchilada")]
#[ignore]
#[named]
#[test]
fn create_upgrade_and_destroy_eks_cluster_with_env_in_eu_west_3() {
    let secrets = FuncTestsSecrets::new();

    let region = secrets.AWS_DEFAULT_REGION.as_ref().expect("AWS region was not found");
    let aws_region = AwsRegion::from_str(region).expect("Wasn't able to parse the desired region");
    let aws_zones = aws_region.get_zones();

    let organization_id = generate_id();
    let cluster_id = generate_cluster_id(aws_region.to_string().as_str());
    let context = context_for_cluster(organization_id, cluster_id, Some(KKind::Eks));

    let cluster_domain = format!(
        "{}.{}",
        to_short_id(&cluster_id),
        secrets
            .DEFAULT_TEST_DOMAIN
            .as_ref()
            .expect("DEFAULT_TEST_DOMAIN is not set in secrets")
            .as_str()
    );

    let environment = helpers::environment::working_minimal_environment(&context);
    let env_action = environment;

    engine_run_test(|| {
        cluster_test(
            function_name!(),
            Kind::Aws,
            KKind::Eks,
            context.clone(),
            logger(),
            metrics_registry(),
            region,
            Some(aws_zones),
            ClusterTestType::WithUpgrade,
            &ClusterDomain::Custom { domain: cluster_domain },
            Some(WithNatGateways),
            CpuArchitecture::AMD64,
            Some(&env_action),
        )
    })
}
