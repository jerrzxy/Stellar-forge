# Multi-Cluster Architecture

This document describes the multi-cluster architecture, including component roles and the communication paths between clusters.

## Components

| Component         | Role                                                     | Scope             |
| ----------------- | -------------------------------------------------------------- | ----------------- |
| Control Plane      | Exposes the API and coordinates operations across clusters       | Cluster-wide       |
| Cluster Agent      | Reports cluster state and receives dispatched work                 | Per cluster       |
| Message Bus        | Reliable event delivery between control plane and agents          | Shared           |
| Storage Layer      | Persists desired state and observed state                         | Shared           |

## Communication Paths

| Source           | Destination      | Protocol       | Direction       |
| ---------------- | --------------- | -------------- | --------------- |
| Control Plane    | Message Bus     | HTTP/gRPC      | Outbound         |
| Message Bus      | Cluster Agent   | gRPC           | Inbound          |
| Cluster Agent    | Storage Layer  | HTTP            | Outbound         |
| Control Plane    | Storage Layer  | HTTP            | Outbound         |

## Failure Handling

| Scenario                | Behavior                                                     |
| ---------------------- | ----------------------------------------------------------------- |
| Agent unreachable       | Retries with exponential backoff; events remain queued                 |
| Message bus outage      | Control plane falls back to direct agent communication                |
| Storage unavailable    | Writes are retried; reads serve last known good state               |
