/**
 *  ---------------------------------------
 *    type stub for external rust modules
 *  ---------------------------------------
 */

export type RgbColor = [number, number, number];
export type FadeInArgs = string | { state: string; durationMs?: number };
export type FadeOutArgs = { durationMs?: number };

export type LedCommand =
  | { command: 'setStaticColor'; args: RgbColor }
  | { command: 'setStripState'; args: string }
  | { command: 'fadeIn'; args: FadeInArgs }
  | { command: 'fadeOut'; args?: FadeOutArgs }
  | { command: 'rainbow' }
  | { command: 'breathing'; args: RgbColor };

export type MessageToServerData =
  | { type: 'led'; data: LedCommand }
  | { type: 'setManualMode'; data: { enabled: boolean } }
  | { type: 'getManualMode' }
  | { type: 'setIdleInhibit'; data: { enabled: boolean; timeoutMs?: number } }
  | { type: 'getIdleInhibit' };

export type MessageToClientData =
  | { type: 'ack' }
  | { type: 'nack'; data: { reason: string } }
  | { type: 'manualMode'; data: { enabled: boolean } }
  | { type: 'idleInhibit'; data: { enabled: boolean; timeoutMs?: number } };

export type MessageToClient = { id: string } & MessageToClientData;
