// Type declarations for @elitesql/client (sidecar mode).

export type QueryResult =
  | { columns: string[]; rows: unknown[][] }
  | { inserted: string[] }
  | { affected: number }
  | { ok: true };

export interface Hit {
  id: string;
  score?: number;
  distance?: number;
  record: Record<string, unknown>;
}

export class EliteSQLError extends Error {
  code: number;
  static CONFLICT_RETRY: number;
  static COMMIT_UNKNOWN: number;
}

export class SidecarClient {
  static connect(socketPath: string): Promise<SidecarClient>;
  static connect(target: { host?: string; port: number; token: string }): Promise<SidecarClient>;
  ping(): Promise<boolean>;
  query(sql: string, params?: unknown[] | Record<string, unknown>): Promise<QueryResult>;
  stream(
    sql: string,
    params?: unknown[] | Record<string, unknown>,
    opts?: { batchRows?: number },
  ): Promise<SidecarQueryCursor>;
  createVectorIndex(
    table: string,
    column: string,
    opts?: { metric?: 'cosine' | 'dot' | 'l2'; mode?: 'sync' | 'async'; m?: number; efConstruction?: number; quantized?: boolean },
  ): Promise<unknown>;
  createTextIndex(table: string, column: string): Promise<unknown>;
  searchVector(
    table: string,
    column: string,
    vector: number[],
    opts?: { topK?: number; efSearch?: number; filter?: Record<string, unknown> },
  ): Promise<Hit[]>;
  searchText(
    table: string,
    column: string,
    query: string,
    opts?: { topK?: number; filter?: Record<string, unknown> },
  ): Promise<Hit[]>;
  searchHybrid(
    table: string,
    opts?: {
      text?: [string, string];
      vector?: [string, number[]];
      topK?: number;
      efSearch?: number;
      filter?: Record<string, unknown>;
    },
  ): Promise<Hit[]>;
  checkpoint(): Promise<unknown>;
  compact(): Promise<unknown>;
  close(): void;
}

export class SidecarQueryCursor implements AsyncIterable<unknown[]> {
  readonly columns: string[];
  readonly done: boolean;
  nextBatch(maxRows?: number): Promise<unknown[][]>;
  close(): Promise<void>;
  [Symbol.asyncIterator](): AsyncIterator<unknown[]>;
}

export function decodeValue(v: unknown): unknown;
export function encodeParam(v: unknown): unknown;
export function encodeParams(v: unknown[] | Record<string, unknown>): unknown;
