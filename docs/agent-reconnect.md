# Stop/start reconnect fix: router 0.26.3, agent 0.2.1

## Router

The primary connection is router → agent over NetBird, not agent call-home.
Previously an outbound WebSocket could exit without unregistering its agent.
The connector treated the cached heartbeat as an existing connection and skipped
all subsequent dials, including after a retained instance was restarted.

Both WebSocket paths now own scoped registrations. Disconnect, error and task
cancellation remove the registration and pending commands/terminals, while
preserving the last-seen timestamp for the existing liveness grace period.
Cleanup from an older socket cannot unregister its replacement. New allocations
invalidate the old socket as well as its cached health; heartbeat processing is
serialized against start/allocation changes. Dials are bounded and deduplicated
per instance. A closed command channel ends the socket rather than spinning.

## Agent

Agent 0.2.1 propagates fatal listener/task errors to a nonzero process exit.
Previously they were logged but reported as success, which could prevent
Supervisor's `autorestart=unexpected` from restarting a failed running service.
This is an independently reproduced defect, not proof that a particular live
instance experienced a listener failure.

## Rollout

- Update the router first. This fix works with existing agent binaries and does
  not require destroying/re-renting a GPU or changing its retained disk.
- GPU images copy the agent at build time. Rebuild them from the immutable
  0.26.3 router digest to include agent 0.2.1. The standalone agent image exports
  the identical binary from that router image.
- Updating the router or changing `latest` does **not** replace the agent inside
  an existing Vast contract. Stop/start also retains that contract's old image.
  Deploying an updated GPU image is a separate, deliberate operation; preserve
  required data and do not destroy a retained instance merely to reconnect it.
- Keep old version tags for rollback. Publication does not restart deployments.

Offline coverage includes WebSocket disconnect/cancellation, replacement-session
cleanup, stop/start with a stale open socket, agent process restart/reconnect and
fatal listener exit status. No real GPU, credentials, cloud lifecycle operations
or model downloads are used by these regression tests.
