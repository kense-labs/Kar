import init, { PiDurable } from "kar-test-bindings";
import { BACKGROUND_CONTEXT as ctx } from "@earendil-works/chord/context";
import { createModels, createProvider } from "@earendil-works/pi-ai/models";
import { anthropicMessagesApi } from "@earendil-works/pi-ai/api/anthropic-messages.lazy";
import { Harness, createRegistry, defineExtension, defineTool } from "@earendil-works/pi-durable";
import { IndexedDbStorage } from "@earendil-works/pi-durable/storage/indexeddb";

const check = (ok, reason) => { if (!ok) throw Error(reason); };
async function rejects(action, why) {
    try { await action(); } catch (error) { return error; }
    throw Error(why);
}
export async function exercise() {
    await init({module_or_path:"/kar_bg.wasm"});
    const models = createModels();
    const baseUrl = location.origin;
    models.setProvider(createProvider({
        id: "fixture", baseUrl, api: anthropicMessagesApi(),
        auth: {apiKey: {name: "fixture", resolve: async () => ({auth:{apiKey:"loopback"},source:"fixture"})}},
        models: [{api:"anthropic-messages",provider:"fixture",baseUrl,id:"local",name:"local",reasoning:false,
            input:["text"],cost:{input:0,output:0,cacheRead:0,cacheWrite:0},contextWindow:65536,maxTokens:512}]
    }));
    const database = "kar-rs-"+crypto.randomUUID();
    const storage = await IndexedDbStorage.open(database);
    const registry = createRegistry();
    const observed = [];
    const parameters = {type:"object", properties:{query:{type:"string"},limit:{type:"number"}}, required:["query","limit"]};
    const childTool = defineTool({name:"lookup_child", description:"Record the nested lookup",
        parameters, async execute(args, api) {
            observed.push(["child",args]);
            api.output("nested lookup complete");
            return {};
        }});
    const rootTool = defineTool({name:"lookup_root", description:"Start a nested lookup",
        parameters, async execute(args, api, context) {
            observed.push(["root",args]);
            const result = await api.executeTool("lookup_child",{query:args.query + " / child",limit:args.limit + 1},context,{key:"child"});
            check(!result.isError,"nested lookup failed");
            return {};
        }});
    registry.install(defineExtension({name:"nested-fixture",tools:[rootTool,childTool]}));
    const options = {models,registry,settings:{retry:{enabled:false},progress:{partialIntervalMs:0}}};
    const harness = await Harness.open(storage,options,ctx);
    const uninitialized = new PiDurable(null, null);
    const caps = uninitialized.sessionCapabilities();
    check(caps.steering && caps.image_input === false && caps.mcp_transports.length === 0 && caps.manual_compaction.kind === "unsupported" && caps.refused_session_options.env, "incorrect Pi Durable pre-open declaration");
    const prompts = caps.resumed_session_options;
    check(!prompts.system_prompt && prompts.append_system_prompt.provided === "immediate" && prompts.append_system_prompt.omitted === "saved", "incorrect native prompt persistence declaration");
    uninitialized.free();
    const runtime = new PiDurable(harness, () => models);
    check(runtime.models().some(m => m.id === "local"),"host model catalog is missing");
    const open = {cwd:"/browser",model:"fixture/local"};
    for (const model of [undefined, {provider:"fixture",modelId:"other"}]) {
        const changed = new PiDurable({createConversation:async()=>({agent:async()=>({model})})}, () => models);
        const e = await rejects(()=>changed.open(open),"accepted an unconfirmed browser model");
        check(e.name === "ModelConfirmationError" && e.requested === open.model &&
            e.reported === (model ? "fixture/other" : null) && !("catalogSelectorIds" in e),
            "lost model confirmation evidence");
    }
    const s = await runtime.open({...open, appendSystemPrompt:"DURABLE-ORIGINAL-RS"});
    const sub = s.subscribe();
    const first = await sub.next();
    check(first.seq === 0 && first.session_id === s.id,"initial snapshot is missing");
    await rejects(()=>s.steer("idle steer"),"accepted a steer while idle");
    const initial = await s.prompt("slow first","first");
    await rejects(()=>s.prompt("extra"),"accepted simultaneous prompt");
    await s.steer("please continue","steer");
    const queued = await s.queue("queued task","queued");
    const withdrawn = await s.queue("unused","withdraw-me");
    await s.withdraw("withdraw-me");
    const stopped = await s.wait(withdrawn.id);
    check(stopped.status==="unanswered","withdrawn submission ran");
    const settled = await s.wait(initial.id);
    const queueResult = await s.wait(queued.id);
    check(settled.status==="done" && queueResult.status==="done","native work did not settle");
    const replay = await s.prompt("same request id","first");
    check(initial.id===replay.id,"native deduplication failed");
    const records = s.records();
    const snapshot = s.snapshot();
    check(snapshot.model === "fixture/local" && snapshot.status.kind === "idle" && snapshot.graph.nodes.includes(s.id), "invalid browser session queries");
    check(snapshot.usage.total?.input === 6 && snapshot.usage.total?.output === 6, "incorrect Pi Durable totals: "+JSON.stringify(snapshot.usage));
    const projected = s.subscribeEvents();
    const opening = await projected.next();
    const afterOpening = s.subscribeEvents(opening.cursor);
    check(JSON.stringify(await afterOpening.next()) === JSON.stringify(await projected.next()), "event cursor skipped a projected sibling");
    const events = records.flatMap(r=>r.events);
    for (const input_id of ["queued", "withdraw-me"]) {
        check(events.some(e=>e.kind==="input_queued" && e.input_id===input_id),"missing queued input "+input_id);
        check(events.some(e=>e.kind==="input_removed" && e.input_id===input_id),"missing settled input "+input_id);
    }
    check(events.some(e=>e.kind==="text_delta" && e.text.includes("browser")), "no assistant text in Rust projection: "+JSON.stringify(events).slice(-2000));
    check(events.some(e=>e.kind==="user_message" && e.text==="slow first"),"no native user message");
    check(events.some(e=>e.kind==="turn_ended" && e.outcome==="completed"),"no projected terminal receipt");
    check(!events.some(e=>e.kind==="input_dropped" && e.input_id==="withdraw-me"),"explicit withdrawal was reported as an interruption");
    check(records.every((r,i)=>r.seq===i && r.stream_id===s.streamId),"invalid cursor sequence");
    const tail = s.subscribe({stream_id:s.streamId,after_seq:records.at(-2).seq});
    check((await tail.next()).seq===records.at(-1).seq,"replay returned the wrong record");
    const abandoned = await s.prompt("slow detached work","detach");
    const id = s.id;
    await s.close();
    while ((await tail.next()) !== undefined) { /* Drain buffered records to the detach boundary. */ }
    const observer = await runtime.open({...open,resume:id});
    check(observer.streamId!==s.streamId && observer.id===id,"failed to attach to existing conversation");
    check((await observer.wait(abandoned.id)).status==="done","closing the controller stopped host-owned work");
    await observer.close();
    const resumed = await runtime.open({...open,resume:id,appendSystemPrompt:"DURABLE-CHANGED-RS"});
    const changed = await resumed.prompt("probe changed instructions","changed-instructions");
    check((await resumed.wait(changed.id)).status==="done","changed-instruction probe failed");
    const abort = await resumed.prompt("slow cancel","cancel");
    const pending = await resumed.queue("must not run","abort-queued");
    await resumed.abort();
    check((await resumed.wait(abort.id)).status==="unanswered","abort did not settle native receipt");
    check((await resumed.wait(pending.id)).status==="unanswered","abort dispatched a waiting follow-up");
    check(resumed.records().flatMap(r=>r.events).some(e=>e.kind==="input_dropped" && e.input_id==="abort-queued"),"interrupted queued input was not reported");
    const malformed = await rejects(()=>runtime.open({...open,resume:"01"}),"accepted malformed conversation id");
    check(malformed.name!=="SessionNotFoundError","malformed id was reported as missing");
    const missing = await rejects(()=>runtime.open({...open,resume:"989898"}),"created a replacement conversation");
    check(missing.name==="SessionNotFoundError" && missing.sessionId==="989898" && missing.cause.method==="Harness.conversation" && missing.cause.native.id===989898,"missing native conversation lost its evidence");
    const unavailable = await rejects(()=>runtime.open({...open,model:"fixture/absent"}),"opened an unregistered model");
    check(unavailable.name==="RuntimeFailureError" && unavailable.failure==="model_unavailable","missing model lost its class");
    await resumed.close();await harness.close(ctx);
    const reopened = await Harness.open(await IndexedDbStorage.open(database),options,ctx);
    const newRuntime = new PiDurable(reopened,models);
    const recovered = await newRuntime.open({...open,resume:id});
    const saved = await recovered.prompt("deduplicated across Harness instances","first");
    check(saved.id===initial.id && saved.status==="done","receipt was not durable across Harness reopen");
    const retained = await recovered.prompt("probe saved instructions","saved-instructions");
    check((await recovered.wait(retained.id)).status==="done","saved-instruction probe failed");
    const lookup = await recovered.prompt("run the nested lookup fixture","lookup");
    check((await recovered.wait(lookup.id)).status==="done","nested tool run failed");
    const rootArgs = {query:'quote " and newline\n',limit:3};
    const childArgs = {query:rootArgs.query + " / child",limit:4};
    check(observed.length===2 && observed[0][0]==="root" && observed[1][0]==="child","unexpected native invocation order");
    const toolEvents = recovered.records().flatMap(r=>r.events);
    for(const [i,id,args] of [[0,"root-lookup",rootArgs],[1,"root-lookup/child",childArgs]]) {
        const inputs = toolEvents.filter(e=>e.kind==="tool_input" && e.id===id);
        check(inputs.length===1 && toolEvents.filter(e=>e.kind==="tool_started" && e.id===id).length===1,
            "expected one start and argument snapshot for "+id+": "+JSON.stringify(toolEvents.filter(e=>e.id===id)));
        for(const actual of [inputs[0].input,observed[i][1]]) {
            check(Object.keys(actual).length===2 && actual.query===args.query && actual.limit===args.limit,
                "native and projected arguments differ for "+id);
        }
    }
    const before = recovered.records().length;
    const auto = await recovered.prompt("without inputId");
    check((await recovered.wait(auto.id)).status==="done","anonymous prompt did not finish");
    const rows = recovered.records();
    const autoIds = rows.slice(before).flatMap(r=>r.prompt ? [r.prompt.input_id] : []);
    check(autoIds.length===1 && /^[0-9a-f-]{36}$/.test(autoIds[0]),"missing generated prompt identity");
    check(rows.filter(r=>r.prompt?.input_id==="lookup").length===1,"opaque supplied identity lost");
    const snap = recovered.snapshot();
    for(const [read,expected] of [[recovered.modelReadback(),snap.model],[recovered.usageReadback(),snap.usage],
        [recovered.contextUsageReadback(),snap.context_usage ?? null]]) {
        check(read.seq===snap.cursor.after_seq && JSON.stringify(read.value)===JSON.stringify(expected),"query cursor or value mismatch");
    }
    await recovered.close();await reopened.close(ctx);
    return {records:records.length,conversation:id};
}
