# Seesaw Insight

Real-time observability and visualization for Seesaw workflows.

## Features

- **Real-time Stream**: Both WebSocket and Server-Sent Events (SSE) for live workflow monitoring
- **Tree Visualization**: Interactive causality tree showing event relationships and effect executions
- **Cursor-based Pagination**: Efficient streaming of large event volumes
- **Dashboard**: Modern web UI with live stream and tree views
- **Stats API**: Current metrics (total events, active effects, recent activity)
- **WebSocket Support**: Bidirectional communication with workflow filtering

## Quick Start

```bash
# Set database URL
export DATABASE_URL="postgres://localhost/seesaw"

# Run the insight server
cargo run -p seesaw-insight

# Server starts at http://127.0.0.1:3000
```

## Architecture

### Stream Table

The `seesaw_stream` table captures workflow lifecycle events:

- `event_dispatched` - New event published to queue
- `effect_started` - Effect execution began
- `effect_completed` - Effect finished successfully
- `effect_failed` - Effect failed with error

Triggers automatically populate this table from `seesaw_events` and `seesaw_effect_executions`.

### SSE Endpoint

`GET /api/stream?cursor=N&limit=100`

Returns a Server-Sent Events stream starting from cursor position. Clients receive JSON events:

```json
{
  "seq": 123,
  "stream_type": "event_dispatched",
  "correlation_id": "...",
  "event_id": "...",
  "payload": {...},
  "created_at": "2026-02-05T10:30:00Z"
}
```

### WebSocket Endpoint

`WS /api/ws?cursor=N&correlation_id=...`

Bidirectional WebSocket connection for real-time updates. Supports:
- Historical entries (via cursor parameter)
- Workflow filtering (via correlation_id parameter)
- Automatic reconnection
- Push-based updates with lower latency than SSE

```javascript
const ws = new WebSocket('ws://localhost:3000/api/ws?cursor=0');
ws.onmessage = (event) => {
    const entry = JSON.parse(event.data);
    console.log(entry);
};
```

### Tree API

`GET /api/tree/:correlation_id`

Returns workflow causality tree showing parent-child event relationships:

```json
[
  {
    "event_id": "...",
    "event_type": "OrderPlaced",
    "created_at": "2026-02-05T10:30:00Z",
    "hops": 0,
    "effects": [
      {
        "effect_id": "validate_order",
        "status": "completed",
        "attempts": 1
      }
    ],
    "children": [
      {
        "event_id": "...",
        "event_type": "OrderValidated",
        "effects": [...],
        "children": [...]
      }
    ]
  }
]
```

### Stats API

`GET /api/stats`

Returns current metrics:

```json
{
  "total_events": 1234,
  "active_effects": 5,
  "recent_entries": 42
}
```

## Configuration

Environment variables:

- `DATABASE_URL` - PostgreSQL connection string (default: `postgres://localhost/seesaw`)
- `BIND_ADDR` - Server address (default: `127.0.0.1:3000`)
- `STATIC_DIR` - Path to static files (auto-detected if not set)

## Library Usage

```rust
use seesaw_insight::{StreamReader, serve};
use sqlx::postgres::PgPoolOptions;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect("postgres://localhost/seesaw")
        .await?;

    // Start web server with dashboard
    serve(pool, "127.0.0.1:3000", Some("./static")).await?;

    Ok(())
}
```

## Schema Setup

The observability schema is included in `docs/schema.sql`. Ensure you've applied the migrations:

```sql
-- Create stream table
CREATE TABLE seesaw_stream (...);

-- Add triggers
CREATE TRIGGER seesaw_events_stream ...;
CREATE TRIGGER seesaw_effect_executions_stream ...;
```

## Performance

- Cursor-based pagination prevents full table scans
- Indexes on `seq` and `correlation_id` for fast lookups
- SSE long-polling with 100ms sleep when no events
- Cap of 1000 entries per request

## Dashboard Features

The web dashboard includes:

- **Live Stream Tab**: Real-time list of all workflow events and effects
- **Tree View Tab**: Click any workflow ID to see its causality tree
- **Connection Toggle**: Switch between SSE and WebSocket
- **Interactive Tree**: Visual representation showing event relationships and effect status
- **Stats Cards**: Auto-refreshing metrics every 5 seconds

## Future Enhancements

Potential future features:

- Workflow pinning/bookmarking
- Log capture and detail panes
- Advanced filtering/search
- Export capabilities
- Performance metrics and charts
- Workflow playback/replay
