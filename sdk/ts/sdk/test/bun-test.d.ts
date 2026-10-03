declare module "bun:test" {
  export interface Matchers {
    toBe(expected: unknown): void;
    toEqual(expected: unknown): void;
    toContain(expected: string): void;
    toBeUndefined(): void;
    toBeNull(): void;
    toBeInstanceOf(expected: unknown): void;
    toBeGreaterThan(expected: number): void;
    toStartWith(expected: string): void;
  }

  export interface Expect {
    <T>(actual: T): Matchers;
  }

  export interface Test {
    (name: string, fn: () => void | Promise<void>, timeout?: number): void;
    skip(name: string, fn?: () => void | Promise<void>, timeout?: number): void;
  }

  export const expect: Expect;
  export function describe(name: string, fn: () => void): void;
  export const test: Test;
}
