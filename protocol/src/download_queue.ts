/** One shared lane across history, live messages and explicit media downloads. */
export class SerialQueue {
  private tail: Promise<unknown> = Promise.resolve();

  constructor(private readonly pauseMs = 0) {}

  run<T>(task: () => Promise<T>): Promise<T> {
    const result = this.tail.then(task);
    this.tail = result.catch(() => {}).then(async () => {
      if (this.pauseMs) {
        await new Promise((resolve) => setTimeout(resolve, this.pauseMs));
      }
    });
    return result;
  }
}
