import { createRequire } from "node:module";
import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import assert from "node:assert/strict";

const repo = fileURLToPath(new URL("../../",import.meta.url));
const deps = resolve(process.env.KAR_BROWSER_DEPS ?? join(repo,"node_modules"));
const require = createRequire(join(deps,"kar-browser-test.cjs"));
const { build } = require("esbuild");
const { chromium } = require("playwright-chromium");
const dir = join(repo,"target/browser");
const result = await build({entryPoints:[join(repo,"tests/browser/host.mjs")],bundle:true,format:"esm",platform:"browser",target:"es2024",write:false,
    alias:{"kar-test-bindings":join(dir,"kar.js")},nodePaths:[deps],logLevel:"silent"});
const wasm = await readFile(join(dir,"kar_bg.wasm"));
let calls = 0;
const prompts = new Map();
const server = createServer(async (req,res)=>{
    if(req.url==="/" || req.url==="/favicon.ico"){res.writeHead(200,{"content-type":"text/html"});return res.end("<!doctype html><title>Kar integration test</title>");}
    if(req.url==="/host.js"){res.writeHead(200,{"content-type":"text/javascript"});return res.end(result.outputFiles[0].contents);}
    if(req.url==="/kar_bg.wasm"){res.writeHead(200,{"content-type":"application/wasm"});return res.end(wasm);}
    if(req.method!=="POST" || !req.url.startsWith("/v1/messages")){res.writeHead(404);return res.end();}
    const chunks=[]; for await(const chunk of req)chunks.push(chunk);
    const body=JSON.parse(Buffer.concat(chunks).toString());
    ++calls;
    const last = JSON.stringify(body.messages.at(-1));
    for (const probe of ["slow first", "probe changed instructions", "probe saved instructions"]) {
        if (last.includes(probe)) prompts.set(probe, JSON.stringify(body.system));
    }
    if (JSON.stringify(body.messages.at(-1)).includes("slow")) await new Promise(ok=>setTimeout(ok,350));
    const lookup = last.includes("run the nested lookup fixture");
    const block = lookup ? {type:"tool_use",id:"root-lookup",name:"lookup_root",input:{}} : {type:"text",text:""};
    const delta = lookup ? {type:"input_json_delta",partial_json:JSON.stringify({query:'quote " and newline\n',limit:3})} : {type:"text_delta",text:"browser answer"};
    const events=[
       {type:"message_start",message:{id:"message-"+calls,type:"message",role:"assistant",model:"local",content:[],stop_reason:null,stop_sequence:null,usage:{input_tokens:3,output_tokens:0}}},
       {type:"content_block_start",index:0,content_block:block},
       {type:"content_block_delta",index:0,delta},
       {type:"content_block_stop",index:0},
       {type:"message_delta",delta:{stop_reason:lookup?"tool_use":"end_turn",stop_sequence:null},usage:{output_tokens:3}},
       {type:"message_stop"}
    ];
    res.writeHead(200,{"content-type":"text/event-stream"});
    res.end(events.map(e=>"event: "+e.type+"\ndata: "+JSON.stringify(e)+"\n\n").join(""));
});
await new Promise(ok=>server.listen(0,"127.0.0.1",ok));
let browser;
try {
    browser = await chromium.launch();
    const page = await browser.newPage();
    page.on("pageerror",err=>console.error("Browser error",err.message));
    await page.goto("http://127.0.0.1:"+server.address().port+"/");
    const verified = await page.evaluate(async()=>await (await import("/host.js")).exercise());
    assert.ok(calls>0);
    for (const [probe, marker] of [["slow first","DURABLE-ORIGINAL-RS"], ["probe changed instructions","DURABLE-CHANGED-RS"], ["probe saved instructions","DURABLE-CHANGED-RS"]]) {
        assert.deepEqual(prompts.get(probe)?.match(/DURABLE-(?:ORIGINAL|CHANGED)-RS/g), [marker], probe);
    }
    console.log("Rust WASM and native Pi Durable Harness verified",verified,"model calls",calls);
} finally {await browser?.close();server.closeAllConnections();await new Promise(ok=>server.close(ok));}
