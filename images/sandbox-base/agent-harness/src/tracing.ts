/**
 * OpenTelemetry tracing for the agent harness.
 *
 * Manual instrumentation only — no auto-instrumentation (too heavy,
 * ESM/esbuild issues).  Uses HTTP/protobuf exporter to avoid native
 * module problems with gRPC + esbuild bundling.
 */

import { NodeSDK } from "@opentelemetry/sdk-node";
import { OTLPTraceExporter } from "@opentelemetry/exporter-trace-otlp-proto";
import { Resource } from "@opentelemetry/resources";
import {
  ATTR_SERVICE_NAME,
  ATTR_SERVICE_NAMESPACE,
} from "@opentelemetry/semantic-conventions";
import {
  trace,
  context,
  propagation,
  SpanStatusCode,
  type Span,
} from "@opentelemetry/api";

const OTEL_ENDPOINT =
  process.env.OTEL_EXPORTER_OTLP_ENDPOINT ?? "http://localhost:4318";

let sdk: NodeSDK | null = null;

export function initTracing(): void {
  if (process.env.OTEL_SDK_DISABLED === "true") return;

  sdk = new NodeSDK({
    resource: new Resource({
      [ATTR_SERVICE_NAME]: "agent-harness",
      [ATTR_SERVICE_NAMESPACE]: "galoy-agents",
    }),
    traceExporter: new OTLPTraceExporter({
      url: `${OTEL_ENDPOINT}/v1/traces`,
    }),
    // No auto-instrumentations — we instrument manually
  });
  sdk.start();
}

export async function shutdownTracing(): Promise<void> {
  await sdk?.shutdown();
}

export function getTracer() {
  return trace.getTracer("agent-harness");
}

export { trace, context, propagation, SpanStatusCode };
export type { Span };
