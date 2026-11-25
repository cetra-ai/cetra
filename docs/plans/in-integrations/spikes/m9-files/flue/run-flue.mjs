// M9 spike D item 1: Flue 2.3.0 as a plain Node script (no CLI) against the loopback fake provider.
import { createProvider } from '@earendil-works/pi-ai';
import { openAICompletionsApi } from '@earendil-works/pi-ai/api/openai-completions.lazy';
import { createOpenTelemetryInstrumentation } from '@flue/opentelemetry';
import { defineTool, init, instrument, setProvider, useModel, useTool } from '@flue/runtime';
import { start } from '@flue/runtime/node';
import { NodeTracerProvider } from '@opentelemetry/sdk-trace-node';
import { SimpleSpanProcessor } from '@opentelemetry/sdk-trace-base';
import { OTLPTraceExporter } from '@opentelemetry/exporter-trace-otlp-proto';
import { readFileSync } from 'node:fs';

const workdir = process.env.M7_OUT;
const base = readFileSync(`${workdir}/base.url`, 'utf8').trim();
const fake = createProvider({
  id: 'fake',
  auth: { apiKey: { name: 'fake (keyless)', resolve: async () => ({ auth: { apiKey: 'sk-fake-m9' } }) } },
  models: [{
    id: 'gpt-4o-mini', name: 'gpt-4o-mini (fake)', api: 'openai-completions', provider: 'fake',
    baseUrl: `${base}/v1`, reasoning: false, input: ['text'],
    cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }, contextWindow: 128000, maxTokens: 4096,
  }],
  api: openAICompletionsApi(),
});
setProvider(fake);

// OTel: the app owns the SDK and the exporter (as the Flue docs require).
const provider = new NodeTracerProvider({
  spanProcessors: [new SimpleSpanProcessor(new OTLPTraceExporter({ url: `${base}/v1/traces` }))],
});
provider.register();
const dispose = instrument(createOpenTelemetryInstrumentation());

const clock = defineTool({
  name: 'get_datetime',
  description: 'Return the current date and time in UTC.',
  run: async () => ({ output: new Date('2026-11-24T12:00:00Z').toISOString() }),
});

export function Demo() {
  useModel('fake/gpt-4o-mini');
  useTool(clock);
  return 'You are a helpful assistant. When asked for the time, call get_datetime.';
}

const flue = await start({ agents: [Demo], env: { ...process.env }, providers: [] });
const h = init(Demo, { id: 'm9-flue-1' });
const receipt = await h.dispatch('What time is it?');
const reply = await h.read(receipt);
console.log('REPLY', JSON.stringify(reply).slice(0, 600));
await dispose();
await flue.stop();
await provider.forceFlush();
await provider.shutdown();
console.log('DONE');
