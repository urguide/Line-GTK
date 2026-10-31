import { assertEquals } from "@std/assert";
import { permissions } from "../src/sandbox.ts";

Deno.test("production worker cannot read message DB, arbitrary files, spawn tools or inspect system", async () => {
  const root = await Deno.makeTempDir();
  try {
    await Deno.mkdir(`${root}/auth`);
    await Deno.mkdir(`${root}/transfer`);
    await Deno.writeTextFile(`${root}/messages.sqlite3`, "private database");
    const code = `self.onmessage=async({data})=>{
      const tests=[()=>Deno.readTextFile(data+'/messages.sqlite3'),()=>Deno.writeTextFile(data+'/outside','bad'),()=>new Deno.Command('echo',{args:['bad']}).output(),()=>Deno.hostname()];
      const denied=[];for(const test of tests){try{await test();denied.push(false);}catch(e){denied.push(e instanceof Deno.errors.NotCapable);}}
      await Deno.writeTextFile(data+'/transfer/allowed','ok');self.postMessage(denied);
    };`;
    // The parent test runner grants only filesystem; network/env are narrowed
    // further here because this test does not need protocol capabilities.
    const worker = new Worker(
      `data:application/typescript,${encodeURIComponent(code)}`,
      {
        type: "module",
        deno: {
          permissions: {
            ...permissions(`${root}/auth`, `${root}/transfer`),
            net: false,
            env: false,
          },
        },
      },
    );
    const result = new Promise<boolean[]>((resolve, reject) => {
      worker.onmessage = ({ data }) => resolve(data);
      worker.onerror = (e) => reject(new Error(e.message));
    });
    worker.postMessage(root);
    try {
      assertEquals(await result, [true, true, true, true]);
      assertEquals(await Deno.readTextFile(`${root}/transfer/allowed`), "ok");
    } finally {
      worker.terminate();
    }
  } finally {
    await Deno.remove(root, { recursive: true });
  }
});
