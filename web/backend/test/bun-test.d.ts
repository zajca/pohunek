declare module "bun:test" {
  export interface Matchers {
    toBe(expected: unknown): void;
    toEqual(expected: unknown): void;
    toBeUndefined(): void;
    toBeInstanceOf(expected: unknown): void;
  }

  export interface Expect {
    <T>(actual: T): Matchers;
  }

  export const expect: Expect;
  export function describe(name: string, fn: () => void): void;
  export function test(name: string, fn: () => void | Promise<void>): void;
}
