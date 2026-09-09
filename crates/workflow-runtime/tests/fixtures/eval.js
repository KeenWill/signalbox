import { defineProgram, evaluation, jsonCodec } from "@signalbox/program-sdk/v1";
const output = jsonCodec((value) => {
    if (typeof value !== "object" || value === null || !("verdicts" in value)
        || !Array.isArray(value.verdicts) || !value.verdicts.every((item) => typeof item === "string")) {
        throw new TypeError("expected trial outcomes");
    }
    return { verdicts: value.verdicts };
});
export default defineProgram({
    input: evaluation.manifest,
    output,
    async run(input) {
        const blob = await evaluation.blob({ digest: input.corpus });
        if (blob.kind !== "answer")
            throw new Error("blob unavailable");
        const corpus = await evaluation.corpus();
        if (corpus.kind !== "answer")
            throw new Error("corpus unavailable");
        const verdicts = [];
        for (let trial = 0; trial < input.cases.length * input.repeats; trial++) {
            const result = await evaluation.judge({ trial });
            if (result.kind !== "answer")
                throw new Error("judge unavailable");
            verdicts.push(result.value.outcome === "verdict" ? result.value.actual : result.value.outcome);
        }
        return { verdicts };
    },
});
