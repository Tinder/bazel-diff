package com.bazel_diff;

import org.junit.Before;
import org.junit.Rule;
import org.junit.Test;
import org.mockito.*;
import org.mockito.junit.*;

import java.io.IOException;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.security.NoSuchAlgorithmException;
import java.util.*;

import static org.junit.Assert.*;
import static org.mockito.Mockito.*;

public class TargetHashingClientImplTests {

    @Mock
    BazelClient bazelClientMock;

    @Mock
    FilesClient filesClientMock;

    List<BazelTarget> defaultTargets;

    @Rule
    public MockitoRule mockitoRule = MockitoJUnit.rule();

    @Before
    public void setUp() throws Exception {
        defaultTargets = new ArrayList<>();
        defaultTargets.add(createRuleTarget("rule1", new ArrayList<>(), "rule1Digest"));
        defaultTargets.add(createRuleTarget("rule2", new ArrayList<>(), "rule2Digest"));
    }

    @Test
    public void hashAllBazelTargets_noTargets() throws IOException {
        when(bazelClientMock.queryAllTargets()).thenReturn(new ArrayList<>());
        TargetHashingClientImpl client = new TargetHashingClientImpl(bazelClientMock, filesClientMock);
        try {
            Map<String, String> hash = client.hashAllBazelTargetsAndSourcefiles(new HashSet<>());
            assertEquals(hash.size(), 0);
        } catch (IOException e) {
            assertTrue(false);
        } catch (NoSuchAlgorithmException e) {
            assertTrue(false);
        }
    }

    @Test
    public void hashAllBazelTargets_ruleTargets() throws IOException {
        when(bazelClientMock.queryAllTargets()).thenReturn(defaultTargets);
        TargetHashingClientImpl client = new TargetHashingClientImpl(bazelClientMock, filesClientMock);
        try {
            Map<String, String> hash = client.hashAllBazelTargetsAndSourcefiles(new HashSet<>());
            assertEquals(2, hash.size());
            assertEquals("2c963f7c06bc1cead7e3b4759e1472383d4469fc3238dc42f8848190887b4775", hash.get("rule1"));
            assertEquals("bdc1abd0a07103cea34199a9c0d1020619136ff90fb88dcc3a8f873c811c1fe9", hash.get("rule2"));
        } catch (IOException | NoSuchAlgorithmException e) {
            fail(e.getMessage());
        }
    }

    @Test
    public void hashAllBazelTargets_ruleTargets_seedFilepaths() throws IOException {
        Set<Path> seedFilepaths = new HashSet<>();
        seedFilepaths.add(Paths.get("somefile.txt"));
        when(filesClientMock.readFile(anyObject())).thenReturn("somecontent".getBytes());
        when(bazelClientMock.queryAllTargets()).thenReturn(defaultTargets);
        TargetHashingClientImpl client = new TargetHashingClientImpl(bazelClientMock, filesClientMock);
        try {
            Map<String, String> hash = client.hashAllBazelTargetsAndSourcefiles(seedFilepaths);
            assertEquals(2, hash.size());
            assertEquals("0404d80eadcc2dbfe9f0d7935086e1115344a06bd76d4e16af0dfd7b4913ee60", hash.get("rule1"));
            assertEquals("6fe63fa16340d18176e6d6021972c65413441b72135247179362763508ebddfe", hash.get("rule2"));
        } catch (IOException | NoSuchAlgorithmException e) {
            fail(e.getMessage());
        }
    }

    @Test
    public void hashAllBazelTargets_ruleTargets_ruleInputs() throws IOException, NoSuchAlgorithmException {
        List<String> ruleInputs = new ArrayList<>();
        ruleInputs.add("rule1");
        BazelTarget rule3 = createRuleTarget("rule3", ruleInputs, "digest");
        defaultTargets.add(rule3);
        BazelTarget rule4 = createRuleTarget("rule4", ruleInputs, "digest2");
        defaultTargets.add(rule4);
        when(bazelClientMock.queryAllTargets()).thenReturn(defaultTargets);
        TargetHashingClientImpl client = new TargetHashingClientImpl(bazelClientMock, filesClientMock);
        try {
            Map<String, String> hash = client.hashAllBazelTargetsAndSourcefiles(new HashSet<>());
            assertEquals(4, hash.size());
            assertEquals("2c963f7c06bc1cead7e3b4759e1472383d4469fc3238dc42f8848190887b4775", hash.get("rule1"));
            assertEquals("bdc1abd0a07103cea34199a9c0d1020619136ff90fb88dcc3a8f873c811c1fe9", hash.get("rule2"));
            assertEquals("87dd050f1ca0f684f37970092ff6a02677d995718b5a05461706c0f41ffd4915", hash.get("rule3"));
            assertEquals("a7bc5d23cd98c4942dc879c649eb9646e38eddd773f9c7996fa0d96048cf63dc", hash.get("rule4"));
        } catch (IOException | NoSuchAlgorithmException e) {
            fail(e.getMessage());
        }
    }

    private BazelTarget createRuleTarget(String ruleName, List<String> ruleInputs, String ruleDigest) throws NoSuchAlgorithmException {
        BazelTarget target = mock(BazelTarget.class);
        BazelRule rule = mock(BazelRule.class);
        when(rule.getName()).thenReturn(ruleName);
        when(rule.getRuleInputList()).thenReturn(ruleInputs);
        when(rule.getDigest()).thenReturn(ruleDigest.getBytes());
        when(target.getRule()).thenReturn(rule);
        when(target.hasRule()).thenReturn(true);
        when(target.hasSourceFile()).thenReturn(false);
        return target;
    }

    private BazelTarget createSourceTarget(String name) {
        BazelTarget target = mock(BazelTarget.class);
        when(target.hasSourceFile()).thenReturn(true);
        when(target.hasRule()).thenReturn(false);
        when(target.getRule()).thenReturn(null);
        when(target.getSourceFileName()).thenReturn(name);
        return target;
    }

    private BazelSourceFileTarget createSourceFileTarget(String name, String digest) throws NoSuchAlgorithmException {
        BazelSourceFileTarget target = mock(BazelSourceFileTarget.class);
        when(target.getName()).thenReturn(name);
        when(target.getDigest()).thenReturn(digest.getBytes());
        return target;
    }
}
