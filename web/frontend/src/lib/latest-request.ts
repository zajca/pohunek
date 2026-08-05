export interface RequestToken {
  readonly generation: number;
  readonly key: string;
}

/** Tracks the latest async request for a view-owned resource key. */
export class LatestRequest {
  private generation = 0;

  begin(key: string): RequestToken {
    this.generation += 1;
    return { generation: this.generation, key };
  }

  invalidate(): void {
    this.generation += 1;
  }

  isCurrent(token: RequestToken, currentKey: string): boolean {
    return token.generation === this.generation && token.key === currentKey;
  }
}
