## Purpose
Measure and communicate transport connectivity and useful record delivery independently, including reasoned reconnects, historical coverage, and recovery time.

## Requirements

### Requirement: Transport and delivery availability are measured independently
The monitor SHALL maintain separate state and uptime measurements for WebSocket transport connectivity and useful text-record delivery for every monitored stream.

#### Scenario: Connected stream delivers records
- **WHEN** the WebSocket is connected and valid text records arrive within the useful-data interval
- **THEN** both transport and delivery availability are up

#### Scenario: Connected stream is silent
- **WHEN** the WebSocket remains connected but no valid text record arrives within the useful-data interval
- **THEN** transport remains distinguishable from delivery, and delivery availability is down with a data-idle reason

#### Scenario: Connection cannot be established
- **WHEN** the monitor cannot establish or maintain the WebSocket transport
- **THEN** transport availability is down with the handshake, read, write, peer-close, or timeout reason and delivery availability is also unavailable

### Requirement: Reconnects retain their initiating reason
The monitor SHALL record why each reconnect occurred and SHALL attribute client-enforced recovery time separately from unexplained server transport downtime.

#### Scenario: Data-idle reconnect
- **WHEN** the useful-data interval expires and the monitor intentionally reconnects
- **THEN** the reconnect is counted with reason `data_idle_timeout`, delivery downtime continues, and the configured reconnect delay is identified as client recovery time

#### Scenario: Transport failure reconnect
- **WHEN** a handshake, socket read/write, peer close, or transport timeout triggers reconnection
- **THEN** the reconnect and downtime are attributed to that transport reason

#### Scenario: Delivery resumes after idle recovery
- **WHEN** a reconnected stream begins delivering valid records
- **THEN** the monitor closes the delivery outage and records its recovery duration without losing the initiating reason

### Requirement: Historical reliability data preserves both availability models
The monitor SHALL persist and return transport duration, delivery duration, reconnect reasons, client recovery duration, message counts, and coverage for each historical interval.

#### Scenario: New historical sample
- **WHEN** the monitor writes a completed aggregation interval
- **THEN** the stored row contains sufficient fields to calculate transport availability and delivery availability independently

#### Scenario: Historical API query
- **WHEN** a client requests a supported history window
- **THEN** the API returns both availability models, reasoned reconnect totals, recovery duration, message totals, and coverage metadata for that window

#### Scenario: Legacy historical row
- **WHEN** a stored row predates the reasoned transport/delivery schema
- **THEN** the API remains able to return the row and marks unavailable classifications as legacy or unknown rather than inventing a cause

### Requirement: Dashboard labels communicate reliability semantics
The dashboard SHALL present transport uptime, delivery uptime, reconnect causes, and client recovery time with labels that do not conflate a reachable silent stream with server transport downtime.

#### Scenario: Prolonged silent stream
- **WHEN** a stream remains transport-reachable but delivers no records for an extended period
- **THEN** the dashboard emphasizes delivery unavailability, shows transport status separately, and groups repeated data-idle reconnects by their explicit reason

#### Scenario: Comparing streams
- **WHEN** an operator compares Messijo with Graze and baseline streams
- **THEN** equivalent transport, delivery, message-rate, reconnect-reason, and coverage measures are shown for the selected time window

#### Scenario: Partial or legacy coverage
- **WHEN** the selected window has missing intervals or legacy rows
- **THEN** the dashboard displays coverage and unknown classification explicitly instead of presenting incomplete values as fully observed uptime
