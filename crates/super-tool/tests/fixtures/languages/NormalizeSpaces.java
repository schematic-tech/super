package texttools;

import tech.schematic.Supertest;
import static tech.schematic.Assumptions.assume;

public final class NormalizeSpaces {
    private NormalizeSpaces() {}

    @Supertest
    public static void normalizingTwiceChangesNothing(String text) {
        assume(text != null);
        String once = Text.collapseSpaces(text);
        String twice = Text.collapseSpaces(once);

        if (!twice.equals(once)) {
            throw new AssertionError("Normalizing twice changed the text.");
        }
    }
}
