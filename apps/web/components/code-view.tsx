/** Server-rendered file view: numbered, anchorable, highlighted lines. Dense on purpose, with no rules between the numbers and the code. */
import { highlightLines } from "@/lib/highlight";

export function CodeView({ text, path }: { text: string; path: string }) {
  const body = text.endsWith("\n") ? text.slice(0, -1) : text;
  const lines = highlightLines(body, path);
  return (
    <div className="overflow-x-auto scrollbar-thin">
      <table className="mono w-full border-collapse text-sm leading-6" aria-label={"Contents of " + path}>
        <thead className="sr-only">
          <tr>
            <th scope="col">Line</th>
            <th scope="col">Code</th>
          </tr>
        </thead>
        <tbody>
          {lines.map((html, i) => (
            <tr key={i} id={"L" + (i + 1)} className="target:bg-accent-soft">
              <td className="w-14 pr-3 pl-4 text-right align-top text-ink-3 select-none">
                <a href={"#L" + (i + 1)} className="num block h-6 hover:text-ink" aria-label={"Line " + (i + 1)}>
                  {i + 1}
                </a>
              </td>
              <td className="pr-4 pl-1 whitespace-pre" dangerouslySetInnerHTML={{ __html: html || " " }} />
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
