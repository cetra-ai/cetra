import { CopyButton } from "@/components/copy-button";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";

const DEFAULT_DATASET = "my-dataset";

function pythonSnippet(name: string): string {
  return `import cetra

cetra.init()  # CETRA_API_KEY: a secret key with the ingest, read and write scopes

def task(input):
    return my_app(input)

def exact(output, expected):
    return output == expected

result = cetra.evaluate(dataset=${name}, task=task, scorers=[exact])
print(result.summary())
`;
}

function typescriptSnippet(name: string): string {
  return `import * as cetra from "@cetra-ai/sdk";

cetra.init(); // CETRA_API_KEY: a secret key with the ingest, read and write scopes

const result = await cetra.evaluate({
  dataset: ${name},
  task: async (input) => myApp(input),
  scorers: [function exact({ output, expected }) { return output === expected; }],
});
console.log(result.summary());
`;
}

/** The code that runs a dataset through `evaluate` (the Python and TypeScript SDKs). */
export function EvaluateSnippet({ datasetName }: { datasetName?: string }) {
  // JSON.stringify quotes and escapes the name for both languages.
  const name = JSON.stringify(datasetName ?? DEFAULT_DATASET);
  const snippets = { python: pythonSnippet(name), typescript: typescriptSnippet(name) };

  return (
    <Tabs defaultValue="python">
      <TabsList>
        <TabsTrigger value="python">Python</TabsTrigger>
        <TabsTrigger value="typescript">TypeScript</TabsTrigger>
      </TabsList>
      <TabsContent value="python">
        <div className="relative">
          <pre className="overflow-x-auto rounded-md border bg-muted p-3 text-xs">
            <code>{snippets.python}</code>
          </pre>
          <div className="absolute top-2 right-2">
            <CopyButton value={snippets.python} label="code" />
          </div>
        </div>
      </TabsContent>
      <TabsContent value="typescript">
        <div className="relative">
          <pre className="overflow-x-auto rounded-md border bg-muted p-3 text-xs">
            <code>{snippets.typescript}</code>
          </pre>
          <div className="absolute top-2 right-2">
            <CopyButton value={snippets.typescript} label="code" />
          </div>
        </div>
      </TabsContent>
    </Tabs>
  );
}
