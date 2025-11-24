/**
 *  ---------------------------------------
 *    type stub for external rust modules
 *  ---------------------------------------
 */

export type RgbColor = [number, number, number];

export type LedCommand =
  | { command: 'setStaticColor'; args: RgbColor }
  | { command: 'setStripState'; args: string }
  | { command: 'fadeIn'; args: string }
  | { command: 'fadeOut' }
  | { command: 'rainbow' }
  | { command: 'breathing'; args: RgbColor };

export type MessageToServerData =
  | { type: 'led'; data: LedCommand }
  | { type: 'setManualMode'; data: { enabled: boolean } }
  | { type: 'getManualMode' };

export type MessageToClientData =
  | { type: 'ack' }
  | { type: 'nack'; data: { reason: string } }
  | { type: 'manualMode'; data: { enabled: boolean } };

export type MessageToClient = { id: string } & MessageToClientData;
