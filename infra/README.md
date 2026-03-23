# Infrastructure

GCP infrastructure for galoy-agents, using [galoy-infra](https://github.com/GaloyMoney/galoy-infra) modules.

## Phases

Infrastructure is rolled out in order. Each phase depends on outputs from the previous one.

### 1. Bootstrap

Sets up foundational GCP resources:
- Enables required GCP APIs
- Creates the Terraform state GCS bucket
- Creates the inception service account

```bash
cd bootstrap
tofu init    # First run: use -backend=false, then configure backend after bucket exists
tofu apply
```

> **Note**: On first run the state bucket doesn't exist yet. Run with
> `-backend-config="prefix=galoy-agents/bootstrap"` using a local backend first,
> then migrate state to GCS after the bucket is created.

### 2. Inception

Sets up networking and compute:
- VPC and subnets
- Bastion host
- GKE node service account
- Backup buckets

```bash
cd inception
tofu init
tofu apply -var="inception_sa=<email from bootstrap output>"
```

### 3. Platform

Sets up the GKE cluster and platform resources:

```bash
cd platform
tofu init
tofu apply -var="node_service_account=<email from inception output>"
```

## Makefile

From the `infra/` directory:

```bash
make bootstrap   # Phase 1
make inception   # Phase 2
make platform    # Phase 3
make fmt         # Format all .tf files
```

## Module Source

All modules reference [galoy-infra](https://github.com/GaloyMoney/galoy-infra) at
commit `4666137` via git source URLs. To upgrade, update the `?ref=` parameter in
each `main.tf`.
