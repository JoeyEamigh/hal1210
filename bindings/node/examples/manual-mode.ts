// Simple manual-mode workflow demo for the hal1210 Node bindings.
// Run with: bun run examples/manual-mode.ts

import type { MessageToClient, MessageToServerData } from '../dist/index.js';
import { Hal1210Client } from '../dist/index.js';

const GET_MANUAL_MODE: MessageToServerData = { type: 'getManualMode' };
const RAINBOW_COMMAND: MessageToServerData = { type: 'led', data: { command: 'rainbow' } };
const TIMEOUT_MS = 2_000;
const RAINBOW_DURATION_MS = 10_000;

async function waitForResponse(client: Hal1210Client, timeoutMs: number): Promise<MessageToClient | null> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error('Timed out waiting for the hal1210 daemon. Is it running?')),
      timeoutMs,
    );

    client
      .nextMessage()
      .then(response => {
        clearTimeout(timer);
        resolve(response);
      })
      .catch(err => {
        clearTimeout(timer);
        reject(err);
      });
  });
}

async function nextMessageWithTimeout(client: Hal1210Client, timeoutMs: number): Promise<MessageToClient> {
  const response = await waitForResponse(client, timeoutMs);
  if (!response) throw new Error('Daemon closed the channel before responding.');
  return response;
}

async function waitForType<TType extends MessageToClient['type']>(
  client: Hal1210Client,
  expectedType: TType,
  timeoutMs: number,
): Promise<Extract<MessageToClient, { type: TType }>> {
  const deadline = Date.now() + timeoutMs;
  while (true) {
    const remaining = deadline - Date.now();
    if (remaining <= 0) throw new Error(`Timed out waiting for ${expectedType} message.`);

    const message = await nextMessageWithTimeout(client, remaining);
    if (message.type === expectedType) return message as Extract<MessageToClient, { type: TType }>;

    console.log(`Received ${message.type} while waiting for ${expectedType}; ignoring.`);
  }
}

async function waitForAck(client: Hal1210Client, expectedId: string): Promise<void> {
  const deadline = Date.now() + TIMEOUT_MS;
  while (true) {
    const remaining = deadline - Date.now();
    if (remaining <= 0) throw new Error(`Timed out waiting for ack ${expectedId}.`);

    const message = await nextMessageWithTimeout(client, remaining);
    if (message.type === 'ack' && message.id === expectedId) return console.log(`Daemon acknowledged ${expectedId}.`);

    if (message.type === 'nack' && message.id === expectedId)
      throw new Error(`Daemon rejected ${expectedId}: ${message.data.reason}`);

    console.log(`Received ${message.type} (${message.id}) while waiting for ack ${expectedId}; continuing.`);
  }
}

async function requestManualModeStatus(client: Hal1210Client, label: string): Promise<boolean> {
  const messageId = client.send(GET_MANUAL_MODE);
  console.log(`Sent manual mode request (${label}) (message id: ${messageId})`);
  const response = await waitForType(client, 'manualMode', TIMEOUT_MS);
  const enabled = response.data.enabled;
  console.log(`${label}: daemon reports manual mode ${enabled ? 'ENABLED' : 'DISABLED'}.`);
  return enabled;
}

async function setManualMode(client: Hal1210Client, enabled: boolean): Promise<void> {
  const request: MessageToServerData = { type: 'setManualMode', data: { enabled } };
  const messageId = client.send(request);
  console.log(`Sent ${enabled ? 'enable' : 'disable'} manual mode request (message id: ${messageId}).`);
  await waitForAck(client, messageId);
}

async function setRainbow(client: Hal1210Client): Promise<void> {
  const messageId = client.send(RAINBOW_COMMAND);
  console.log(`Sent rainbow command (message id: ${messageId}).`);
  await waitForAck(client, messageId);
}

function sleep(ms: number): Promise<void> {
  return new Promise(resolve => setTimeout(resolve, ms));
}

async function main(): Promise<void> {
  const client = await Hal1210Client.connect(true);
  try {
    await requestManualModeStatus(client, 'Initial status');

    await setManualMode(client, true);
    const enabled = await requestManualModeStatus(client, 'Post-enable status');
    if (!enabled) throw new Error('Daemon did not report manual mode as enabled.');

    await setRainbow(client);
    console.log(`Rainbow effect running for ${RAINBOW_DURATION_MS / 1000} seconds...`);
    await sleep(RAINBOW_DURATION_MS);

    await setManualMode(client, false);
    console.log('Manual mode disabled; disconnecting.');
  } finally {
    client.cancel();
  }
}

main().catch(err => {
  console.error('Example failed:', err);
  process.exitCode = 1;
  process.exit();
});
