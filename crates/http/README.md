# omp-http

`omp-http` owns OMP's process-wide outbound Reqwest clients and Rustls provider policy.
It exposes cheap `Client` clones that share connection pools across app, driver, and
environment-host call sites.

## Philosophy

TLS configuration and connection-pool lifetime are process policy, not request policy.
Callers select the shared default or redirect-disabled client; specialized clients start
from the provider-aware builder and remain owned by the subsystem defining that policy.
