# Docker development environment

The files in this directory are only for development and regression testing.
Docker Compose and container images are not supported production deployment
artifacts. Install a native `tar.gz`, `deb`, or `rpm` package for measurements.

Run the basic environment from the repository root:

```powershell
docker compose -f docker/compose.yaml -f docker/compose.managed-direct.yaml build
docker compose -f docker/compose.yaml -f docker/compose.managed-direct.yaml up -d
```
