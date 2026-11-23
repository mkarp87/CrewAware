package main

import "context"

// Transport delivers unit positions and repeater check-ins to the engine.
// Implementations: the simulator, and (later) the MeshCore companion over USB serial.
type Transport interface {
	Run(ctx context.Context, e *Engine)
}
