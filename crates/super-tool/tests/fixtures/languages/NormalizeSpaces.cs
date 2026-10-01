using System;
using Schematic;

namespace TextTools;

public static class NormalizeSpaces
{
    [Supertest]
    public static void NormalizingTwiceChangesNothing(string text)
    {
        var once = Text.CollapseSpaces(text);
        var twice = Text.CollapseSpaces(once);

        if (twice != once)
        {
            throw new InvalidOperationException("Normalizing twice changed the text.");
        }
    }
}
