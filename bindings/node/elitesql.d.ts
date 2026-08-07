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
}

export class SidecarClient {
  static connect(socketPath: string): Promise<SidecarClient>;
  ping(): Promise<boolean>;
  query(sql: string): Promise<QueryResult>;
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

export function decodeValue(v: unknown): unknown;
